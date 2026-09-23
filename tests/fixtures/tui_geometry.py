"""Exercise TUI header and kitty geometry through a controlling PTY."""
import codecs
import fcntl
import json
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import time


class Screen:
    def __init__(self, width, height):
        self.resize(width, height)
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")()

    def resize(self, width, height):
        self.width = width
        self.height = height
        self.cells = [[" "] * width for _ in range(height)]
        self.row = 0
        self.column = 0

    def render(self, data):
        self.pending += self.decoder.decode(data)
        while self.pending:
            if self.pending.startswith("\x1b"):
                sequence = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                if sequence is None:
                    return
                parameters, _, command = sequence.groups()
                self.pending = self.pending[sequence.end():]
                if parameters.startswith("?"):
                    continue
                values = [int(value or "0") for value in parameters.split(";")]
                amount = values[0] or 1
                if command in ("H", "f"):
                    self.row = amount - 1
                    self.column = (values[1] or 1) - 1 if len(values) > 1 else 0
                elif command == "G":
                    self.column = amount - 1
                elif command == "d":
                    self.row = amount - 1
                elif command == "A":
                    self.row -= amount
                elif command == "B":
                    self.row += amount
                elif command == "C":
                    self.column += amount
                elif command == "D":
                    self.column -= amount
                elif command == "J" and values[0] in (2, 3):
                    for line in self.cells:
                        line[:] = [" "] * self.width
                elif command == "K" and 0 <= self.row < self.height:
                    start = self.column if values[0] == 0 else 0
                    end = self.column + 1 if values[0] == 1 else self.width
                    self.cells[self.row][start:end] = [" "] * (end - start)
            else:
                character, self.pending = self.pending[0], self.pending[1:]
                if character == "\r":
                    self.column = 0
                elif character == "\n":
                    self.row += 1
                elif character.isprintable():
                    if 0 <= self.row < self.height and 0 <= self.column < self.width:
                        self.cells[self.row][self.column] = character
                    self.column += 1

    def text(self):
        return "\n".join("".join(line) for line in self.cells)

    def find(self, fragment):
        for index, line in enumerate(self.cells):
            if fragment in "".join(line):
                return index
        return None


def main():
    binary, root = sys.argv[1:]
    config_path = os.path.join(root, "config.json")
    with open(config_path, "w", encoding="utf-8") as file:
        json.dump({
            "workspace": root,
            "sessions_dir": os.path.join(root, "sessions"),
            "providers": {"o": {
                "kind": "openrouter", "base_url": "http://127.0.0.1:1",
                "api_key_env": None, "timeout_seconds": 1,
            }},
            "model": {"provider": "o", "model": "m", "max_tokens": 4096},
            "agents": [{"name": "plan", "default": True, "prompt": "Plan."}],
            "theme": "diet_soda",
        }, file)

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    before = termios.tcgetattr(slave)

    def attach_controlling_terminal():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    process = subprocess.Popen(
        [binary, "--config", config_path], stdin=slave, stdout=slave, stderr=slave,
        env={**os.environ, "TERM": "xterm-256color"}, preexec_fn=attach_controlling_terminal,
    )
    screen = Screen(100, 24)
    raw = b""

    def wait_for(predicate, label):
        nonlocal raw
        deadline = time.monotonic() + 12
        while time.monotonic() < deadline:
            ready, _, _ = select.select([master], [], [], 0.1)
            if ready:
                data = os.read(master, 65536)
                raw += data
                screen.render(data)
            if predicate():
                return
            if process.poll() is not None:
                raise AssertionError("process exited while waiting for %s: %r" % (label, raw))
        raise AssertionError("missing %s\n%s" % (label, screen.text()))

    def resize(width, height):
        screen.resize(width, height)
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))

    try:
        wait_for(
            lambda: screen.find("Input") is not None
            and screen.find("Enter Send") is not None,
            "initial complete frame",
        )
        top = "o:m | agent plan | effort default"
        second = "$0.0000 | context 0/4096"
        assert screen.find(top) == 1, "top metadata was not the second terminal row\n%s" % screen.text()
        assert screen.find(second) == 2, "spend/context metadata was not the third terminal row\n%s" % screen.text()
        assert "agent: plan | model:" not in screen.text()

        first_rows = ("      /█   /█", " ∩ ∩   Ω", "         Z")
        kitty_signature = set("█▄▀Ω∩Z/")
        # 14 is the widest variant's canvas width and is deliberately a conservative
        # scan floor so reserved-region detection covers every variant. A future
        # variant wider than 14 cells must raise this constant or reservation_left
        # will be silently invalidated.
        kitty_canvas_width = 14
        reservation_left = screen.width - 2 - kitty_canvas_width + 1

        def kitty_cells(exclude_metadata=False):
            input_top = screen.find("Input")
            if input_top is None:
                footer_top = screen.find("Enter Send")
                if footer_top is None:
                    footer_top = screen.find("/help Commands")
                input_top = footer_top if footer_top is not None else screen.height - 2
            content_rows = range(1, input_top)
            # The artwork is right-aligned with its last column at width - 2.
            left = screen.width - 2 - kitty_canvas_width + 1
            cells = [
                (row, column, character)
                for row, line in enumerate(screen.cells)
                for column, character in enumerate(line)
                if row in content_rows and column >= left and character in kitty_signature
            ]
            if exclude_metadata:
                metadata_ranges = []
                for metadata, metadata_row in ((top, 1), (second, 2)):
                    line = "".join(screen.cells[metadata_row])
                    if metadata in line:
                        start = line.index(metadata)
                        metadata_ranges.append((metadata_row, start, start + len(metadata)))
                cells = [
                    cell for cell in cells
                    if not any(
                        row == metadata_row and start <= column < end
                        for metadata_row, start, end in metadata_ranges
                        for row, column, _ in (cell,)
                    )
                ]
            return cells

        row_text = "".join(screen.cells[1][reservation_left:])
        matching = [row for row in first_rows if row.strip() in row_text]
        assert len(matching) == 1, "kitty first row was not content-top aligned: %r" % [
            (index, character) for index, character in enumerate(screen.cells[1]) if character != " "
        ]
        variant_names = {
            first_rows[0]: "Blob",
            first_rows[1]: "Cbear",
            first_rows[2]: "Fly Girl",
        }
        variant = variant_names[matching[0]]
        initial_kitty = kitty_cells(exclude_metadata=True)
        assert initial_kitty, "kitty artwork was not painted in the reserved right region"
        kitty_rows = {row for row, _, _ in initial_kitty}
        detected_kitty = [
            cell for cell in kitty_cells(exclude_metadata=True) if cell[0] in kitty_rows
        ]
        assert detected_kitty, "kitty artwork was not detected across its painted rows"
        kitty_left = min(column for _, column, _ in detected_kitty)
        first_painted_row = min(row for row, _, _ in initial_kitty)
        assert first_painted_row == 1, "kitty did not start on the content row"
        initial_rightmost = max(column for _, column, _ in initial_kitty)
        assert initial_rightmost == screen.width - 2, "kitty did not reach the content right edge"

        wide_divider = screen.cells[3]
        wide_divider_glyphs = {wide_divider[1], wide_divider[2]}
        wide_divider_columns = [
            column for column, character in enumerate(wide_divider) if character in wide_divider_glyphs
        ]
        assert wide_divider_columns, "wide history divider was not drawn"
        assert min(wide_divider_columns) == 1, "wide history divider did not start at the history left edge"
        wide_divider_end = max(wide_divider_columns)
        assert wide_divider_end < kitty_left, (
            "wide history divider reached the kitty reservation: end=%d kitty_left=%d"
            % (wide_divider_end, kitty_left)
        )
        assert all(
            character not in wide_divider_glyphs
            for character in wide_divider[kitty_left:]
        ), "wide history divider glyph appeared at or beyond the kitty reservation"

        metadata_ranges = []
        for metadata, metadata_row in ((top, 1), (second, 2)):
            metadata_line = "".join(screen.cells[metadata_row])
            metadata_start = metadata_line.find(metadata)
            assert metadata_start >= 0, "metadata was not located on its expected row"
            metadata_end = metadata_start + len(metadata)
            assert metadata_end > metadata_start, "located metadata occupied no cells"
            metadata_ranges.append((metadata, metadata_row, metadata_start, metadata_end))
            assert kitty_left > metadata_end, (
                "kitty left edge was not strictly after metadata: "
                "variant=%s kitty_left=%d metadata_end=%d line=%r"
                % (variant, kitty_left, metadata_end, metadata_line)
            )
            assert not any(
                row == metadata_row and metadata_start <= column < metadata_start + len(metadata)
                for row, column, _ in initial_kitty
            ), "kitty glyph shared a cell with header metadata"

        print(
            "variant=%s kitty_left=%d metadata_end=%d divider_end=%d"
            % (variant, kitty_left, max(end for _, _, _, end in metadata_ranges), wide_divider_end)
        )

        initial_anchor = (first_painted_row, initial_rightmost)

        def kitty_at_anchor():
            cells = kitty_cells()
            return bool(cells) and (min(row for row, _, _ in cells), max(column for _, column, _ in cells)) == initial_anchor

        resize(40, 18)
        wait_for(
            lambda: screen.find("effort default") is not None
            and screen.find("Enter Send") is not None
            and not kitty_at_anchor(),
            "narrow redraw without kitty",
        )
        assert screen.find("effort default") == 1
        assert screen.find("context") == 2
        assert not kitty_cells(exclude_metadata=True), "kitty glyphs remained after narrow redraw"
        narrow_divider = screen.cells[3]
        narrow_divider_glyphs = {narrow_divider[1], narrow_divider[2]}
        narrow_divider_glyphs.add(narrow_divider[screen.width - 2])
        narrow_divider_columns = [
            column for column, character in enumerate(narrow_divider) if character in narrow_divider_glyphs
        ]
        assert narrow_divider_columns, "narrow history divider was not drawn"
        assert min(narrow_divider_columns) == 1, "narrow history divider did not start at the history left edge"
        narrow_divider_end = max(narrow_divider_columns)
        assert narrow_divider_end == screen.width - 2, (
            "narrow history divider did not span the full history band: end=%d width=%d"
            % (narrow_divider_end, screen.width)
        )
        assert narrow_divider[narrow_divider_end] in ("╮", "+"), (
            "narrow history divider did not terminate with a top-right corner: %r"
            % narrow_divider[narrow_divider_end]
        )

        resize(100, 24)
        wait_for(
            lambda: screen.find(top) == 1
            and screen.find("Enter Send") is not None
            and kitty_at_anchor(),
            "restored kitty anchor",
        )
        restored_kitty = kitty_cells()
        restored_anchor = (
            min(row for row, _, _ in restored_kitty),
            max(column for _, column, _ in restored_kitty),
        )
        assert restored_anchor == initial_anchor

        os.write(master, b"/quit\r")
        wait_for(lambda: process.poll() == 0, "clean quit")
        assert b"\x1b[?1000l" in raw, "mouse capture was not disabled"
        assert b"\x1b[?1006l" in raw, "SGR mouse mode was not disabled"
        assert b"\x1b[?2004l" in raw, "bracketed paste mode was not disabled"
        assert before == termios.tcgetattr(slave), "terminal modes were not restored"
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)


if __name__ == "__main__":
    main()
