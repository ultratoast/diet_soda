//! Single-pass variable substitution with no evaluation or recursive expansion.
//! Whole-value JSON placeholders preserve types instead of stringifying objects.
use anyhow::{Context, Result};
use serde_json::Value;

pub fn render(text: &str, vars: &Value) -> Result<String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let end = rest[start + 2..]
            .find("}}")
            .context("Unclosed template variable")?
            + start
            + 2;
        let key = rest[start + 2..end].trim();
        let v = vars
            .get(key)
            .with_context(|| format!("Undefined template variable: {key}"))?;
        out.push_str(&match v {
            Value::String(s) => s.clone(),
            v => v.to_string(),
        });
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

pub fn render_json(value: &Value, vars: &Value) -> Result<Value> {
    Ok(match value {
        Value::String(s) => {
            if s.starts_with("{{") && s.ends_with("}}") && s.matches("{{").count() == 1 {
                let key = s[2..s.len() - 2].trim();
                vars.get(key)
                    .with_context(|| format!("Undefined template variable: {key}"))?
                    .clone()
            } else {
                Value::String(render(s, vars)?)
            }
        }
        Value::Array(a) => Value::Array(
            a.iter()
                .map(|v| render_json(v, vars))
                .collect::<Result<_>>()?,
        ),
        Value::Object(o) => Value::Object(
            o.iter()
                .map(|(k, v)| Ok((k.clone(), render_json(v, vars)?)))
                .collect::<Result<_>>()?,
        ),
        v => v.clone(),
    })
}
