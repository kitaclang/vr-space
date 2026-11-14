//! Versioned CLI control grammar and JSON protocol envelopes.

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Subcommand)]
pub enum TopCommand {
    Control(ControlArgs),
}

#[derive(Debug, Clone, Args)]
pub struct ControlArgs {
    #[command(subcommand)]
    pub command: ControlCommand,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ControlCommand {
    Status {
        #[arg(long)]
        json: bool,
    },
    Recenter,
    Horizon {
        state: OnOff,
    },
    Screen {
        #[command(subcommand)]
        action: ScreenCommand,
    },
    Roll {
        #[command(subcommand)]
        action: RollCommand,
    },
    ShowDisplay {
        id: u32,
    },
    ShowWindow {
        id: u32,
    },
    SecondDisplay {
        id: u32,
    },
    SecondWindow {
        id: u32,
    },
    Second {
        state: SecondState,
    },
    Fullscreen {
        target: Option<String>,
    },
    Hud {
        state: OnOff,
    },
    Prediction {
        value: String,
    },
    CopyFrames {
        state: OnOff,
    },
    OpenScreenSettings,
    OpenAtLogin {
        state: OnOff,
    },
    Quit,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum SecondState {
    Off,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ScreenCommand {
    Bigger,
    Smaller,
    Further,
    Closer,
    Reset,
    Curvature { value: f32 },
}

#[derive(Debug, Clone, Subcommand)]
pub enum RollCommand {
    Left,
    Right,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Status,
    Recenter,
    Horizon {
        on: bool,
    },
    Further,
    Closer,
    Bigger,
    Smaller,
    ResetScreen,
    RollLeft,
    RollRight,
    SetCurve {
        value: f32,
    },
    ShowDisplay {
        id: u32,
    },
    ShowWindow {
        id: u32,
    },
    SecondDisplay {
        id: u32,
    },
    SecondWindow {
        id: u32,
    },
    HideSecondScreen,
    Fullscreen {
        id: Option<u32>,
    },
    Hud {
        on: bool,
    },
    Prediction {
        milliseconds: Option<f32>,
        on: Option<bool>,
    },
    PredictionMore,
    PredictionLess,
    CopyFrames {
        on: bool,
    },
    OpenScreenSettings,
    OpenAtLogin {
        on: bool,
    },
    Quit,
}

impl ControlCommand {
    pub fn action(&self) -> anyhow::Result<Action> {
        use Action as A;
        Ok(match self {
            Self::Status { .. } => A::Status,
            Self::Recenter => A::Recenter,
            Self::Horizon { state } => A::Horizon {
                on: matches!(state, OnOff::On),
            },
            Self::Screen { action } => match action {
                ScreenCommand::Curvature { value }
                    if value.is_finite() && (0.0..=1.0).contains(value) =>
                {
                    A::SetCurve { value: *value }
                }
                ScreenCommand::Curvature { .. } => {
                    anyhow::bail!("curvature must be finite and between 0 and 1")
                }
                ScreenCommand::Bigger => A::Bigger,
                ScreenCommand::Smaller => A::Smaller,
                ScreenCommand::Further => A::Further,
                ScreenCommand::Closer => A::Closer,
                ScreenCommand::Reset => A::ResetScreen,
            },
            Self::Roll { action } => match action {
                RollCommand::Left => A::RollLeft,
                RollCommand::Right => A::RollRight,
            },
            Self::ShowDisplay { id } => A::ShowDisplay { id: *id },
            Self::ShowWindow { id } => A::ShowWindow { id: *id },
            Self::SecondDisplay { id } => A::SecondDisplay { id: *id },
            Self::SecondWindow { id } => A::SecondWindow { id: *id },
            Self::Second { .. } => A::HideSecondScreen,
            Self::Fullscreen { target } => A::Fullscreen {
                id: match target.as_deref() {
                    None | Some("window") => None,
                    Some(s) => Some(parse_id(s)?),
                },
            },
            Self::Hud { state } => A::Hud {
                on: matches!(state, OnOff::On),
            },
            Self::Prediction { value } => match value.as_str() {
                "toggle" => A::Prediction {
                    milliseconds: None,
                    on: None,
                },
                "more" => A::PredictionMore,
                "less" => A::PredictionLess,
                "on" => A::Prediction {
                    milliseconds: None,
                    on: Some(true),
                },
                "off" => A::Prediction {
                    milliseconds: None,
                    on: Some(false),
                },
                _ => {
                    let ms: f32 = value.parse().map_err(|_| anyhow::anyhow!("prediction must be toggle, more, less, on, off, or 0..100 milliseconds"))?;
                    if ms.is_finite() && (0.0..=100.0).contains(&ms) {
                        A::Prediction {
                            milliseconds: Some(ms),
                            on: None,
                        }
                    } else {
                        anyhow::bail!("prediction must be finite and between 0 and 100 ms")
                    }
                }
            },
            Self::CopyFrames { state } => A::CopyFrames {
                on: matches!(state, OnOff::On),
            },
            Self::OpenScreenSettings => A::OpenScreenSettings,
            Self::OpenAtLogin { state } => A::OpenAtLogin {
                on: matches!(state, OnOff::On),
            },
            Self::Quit => A::Quit,
        })
    }
}

fn parse_id(s: &str) -> anyhow::Result<u32> {
    s.parse::<u32>()
        .map_err(|_| anyhow::anyhow!("display ID must be an unsigned integer"))
}

pub fn request(action: Action) -> Value {
    json!({"version": 1, "command": action})
}

pub fn decode_request(value: Value) -> Result<Action, String> {
    if value.get("version").and_then(Value::as_u64) != Some(1) {
        return Err("unsupported or missing protocol version (expected 1)".into());
    }
    let action: Action =
        serde_json::from_value(value.get("command").cloned().ok_or("missing command")?)
            .map_err(|e| format!("invalid command: {e}"))?;
    match &action {
        Action::SetCurve { value } if !value.is_finite() || !(0.0..=1.0).contains(value) => {
            return Err("curvature must be finite and between 0 and 1".into())
        }
        Action::Prediction {
            milliseconds: Some(_),
            on: Some(_),
        } => return Err("prediction request cannot set both milliseconds and on/off".into()),
        Action::Prediction {
            milliseconds: Some(ms),
            ..
        } if !ms.is_finite() || !(0.0..=100.0).contains(ms) => {
            return Err("prediction must be finite and between 0 and 100 ms".into())
        }
        Action::ShowWindow { id: 0 } | Action::SecondWindow { id: 0 } => {
            return Err("window ID 0 is invalid".into())
        }
        _ => {}
    }
    Ok(action)
}

pub fn success(result: Value) -> Value {
    json!({"version": 1, "ok": true, "result": result})
}
pub fn failure(error: impl ToString) -> Value {
    json!({"version": 1, "ok": false, "error": error.to_string()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: TopCommand,
    }

    #[test]
    fn parses_controls_and_validates_numbers() {
        let status = TestCli::try_parse_from(["vr-space", "control", "status", "--json"]).unwrap();
        let TopCommand::Control(status) = status.command;
        assert!(matches!(
            status.command,
            ControlCommand::Status { json: true }
        ));
        let cli = TestCli::try_parse_from(["vr-space", "control", "screen", "bigger"]).unwrap();
        let TopCommand::Control(c) = cli.command;
        assert_eq!(c.command.action().unwrap(), Action::Bigger);
        let curve =
            TestCli::try_parse_from(["vr-space", "control", "screen", "curvature", "0.5"]).unwrap();
        let TopCommand::Control(c) = curve.command;
        assert_eq!(c.command.action().unwrap(), Action::SetCurve { value: 0.5 });
        let login =
            TestCli::try_parse_from(["vr-space", "control", "open-at-login", "on"]).unwrap();
        let TopCommand::Control(c) = login.command;
        assert_eq!(
            c.command.action().unwrap(),
            Action::OpenAtLogin { on: true }
        );
        for (value, expected) in [
            (
                "toggle",
                Action::Prediction {
                    milliseconds: None,
                    on: None,
                },
            ),
            ("more", Action::PredictionMore),
            ("less", Action::PredictionLess),
            (
                "off",
                Action::Prediction {
                    milliseconds: None,
                    on: Some(false),
                },
            ),
            (
                "10",
                Action::Prediction {
                    milliseconds: Some(10.0),
                    on: None,
                },
            ),
        ] {
            let cli =
                TestCli::try_parse_from(["vr-space", "control", "prediction", value]).unwrap();
            let TopCommand::Control(c) = cli.command;
            assert_eq!(c.command.action().unwrap(), expected);
        }
    }

    #[test]
    fn request_round_trip_and_version_validation() {
        let a = Action::ShowWindow { id: 42 };
        assert_eq!(decode_request(request(a.clone())).unwrap(), a);
        assert!(decode_request(json!({"version":2,"command":{"action":"status"}})).is_err());
        assert!(
            decode_request(json!({"version":1,"command":{"action":"set_curve","value":1.1}}))
                .is_err()
        );
        assert!(
            decode_request(json!({"version":1,"command":{"action":"show_window","id":0}})).is_err()
        );
        assert!(decode_request(
            json!({"version":1,"command":{"action":"prediction","milliseconds":101.0,"on":null}})
        )
        .is_err());
        assert!(decode_request(
            json!({"version":1,"command":{"action":"prediction","milliseconds":10.0,"on":true}})
        )
        .is_err());
        let ok = success(json!({"capture":{"primary":"waiting_for_frames"}}));
        assert_eq!(ok["version"], 1);
        assert_eq!(ok["ok"], true);
        assert_eq!(ok["result"]["capture"]["primary"], "waiting_for_frames");
    }
}
