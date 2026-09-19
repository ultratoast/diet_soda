//! Provider-independent effort levels with explicit per-model opt-in.
use super::{ModelConfig, ProviderKind};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}
impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for Effort {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "none" | "off" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            _ => bail!("Unknown effort: {s}; use none, minimal, low, medium, high, xhigh, or max"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReasoningConfig {
    pub supported_efforts: Vec<Effort>,
    /// None leaves the provider's default unchanged until /effort is used.
    pub effort: Option<Effort>,
}
impl ReasoningConfig {
    pub fn validate(&self, provider: &ProviderKind) -> Result<()> {
        if self.supported_efforts.is_empty() {
            bail!("reasoning.supported_efforts must not be empty");
        }
        if let Some(effort) = self.effort {
            if !self.supported_efforts.contains(&effort) {
                bail!("Effort {effort} is not supported by this model");
            }
        }
        if *provider == ProviderKind::Anthropic
            && self
                .supported_efforts
                .iter()
                .any(|e| matches!(e, Effort::None | Effort::Minimal))
        {
            bail!("Anthropic output_config.effort accepts low, medium, high, xhigh, or max");
        }
        Ok(())
    }
}
impl ModelConfig {
    pub fn set_effort(&mut self, effort: Effort) -> Result<()> {
        let reasoning = self.reasoning.as_mut().context("Reasoning effort is not configured for this model; declare its reasoning.supported_efforts in config.json")?;
        if !reasoning.supported_efforts.contains(&effort) {
            bail!(
                "Effort {effort} is unsupported; supported: {}",
                reasoning
                    .supported_efforts
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        reasoning.effort = Some(effort);
        Ok(())
    }
}
