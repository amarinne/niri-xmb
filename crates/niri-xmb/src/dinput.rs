//! Flydigi Vader 4 Pro DInput report decoder.
//!
//! This is the narrow, reusable part of the DInput stack: decode the 32-byte
//! extended report into a stable state. Transport, hotplug, virtual-gamepad
//! output, and navi policy live in niri-xmb, not here.
//!
//! Byte layout and button positions were adapted from InputPlumber's
//! `flydigi_vader_4_pro` driver (GPL-3.0-or-later) and cross-checked against
//! Vader4ProReader (Unlicense) and padctl's Vader 4 DInput config. The code is
//! a manual implementation, not a copy of either runtime. See
//! `DINPUT_SPIKE.md` for source URLs, commits, and the unresolved hardware
//! checks.

use serde::Serialize;
use std::fmt;

pub const VADER4_DINPUT_VID: u16 = 0x04b4;
pub const VADER4_DINPUT_PID: u16 = 0x2412;
pub const VADER4_DINPUT_REPORT_ID: u8 = 0x04;
pub const VADER4_DINPUT_REPORT_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    Length { got: usize },
    ReportId { got: u8 },
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Length { got } => write!(
                formatter,
                "expected a {VADER4_DINPUT_REPORT_LEN}-byte Vader 4 report, got {got}"
            ),
            Self::ReportId { got } => write!(
                formatter,
                "expected report ID {VADER4_DINPUT_REPORT_ID:#04x}, got {got:#04x}"
            ),
        }
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Stick {
    pub x: u8,
    pub y: u8,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Imu {
    pub x: i16,
    pub y: i16,
    pub z: i16,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DPad {
    pub up: bool,
    pub right: bool,
    pub down: bool,
    pub left: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Buttons {
    pub a: bool,
    pub b: bool,
    pub x: bool,
    pub y: bool,
    pub lb: bool,
    pub rb: bool,
    pub lt: bool,
    pub rt: bool,
    pub ls: bool,
    pub rs: bool,
    pub select: bool,
    pub start: bool,
    pub guide: bool,
    pub r#fn: bool,
    pub m1: bool,
    pub m2: bool,
    pub m3: bool,
    pub m4: bool,
    pub c: bool,
    pub z: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Vader4State {
    pub report_id: u8,
    pub buttons: Buttons,
    pub dpad: DPad,
    pub left_stick: Stick,
    pub right_stick: Stick,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub gyro: Imu,
    pub accel: Imu,
}

impl Vader4State {
    /// Decode one extended report. Byte 0 is the report ID; the remaining
    /// bytes follow the DInput/2.4G map.
    pub fn parse(report: &[u8]) -> Result<Self, ParseError> {
        if report.len() != VADER4_DINPUT_REPORT_LEN {
            return Err(ParseError::Length { got: report.len() });
        }
        if report[0] != VADER4_DINPUT_REPORT_ID {
            return Err(ParseError::ReportId { got: report[0] });
        }

        let extra = report[7];
        let system = report[8];
        let face = report[9];
        let shoulders = report[10];

        Ok(Self {
            report_id: report[0],
            buttons: Buttons {
                a: face & 0x10 != 0,
                b: face & 0x20 != 0,
                x: shoulders & 0x01 != 0,
                y: face & 0x80 != 0,
                lb: shoulders & 0x04 != 0,
                rb: shoulders & 0x08 != 0,
                lt: shoulders & 0x10 != 0,
                rt: shoulders & 0x20 != 0,
                ls: shoulders & 0x40 != 0,
                rs: shoulders & 0x80 != 0,
                select: face & 0x40 != 0,
                start: shoulders & 0x02 != 0,
                guide: system & 0x08 != 0,
                r#fn: system & 0x01 != 0,
                m1: extra & 0x04 != 0,
                m2: extra & 0x08 != 0,
                m3: extra & 0x10 != 0,
                m4: extra & 0x20 != 0,
                c: extra & 0x01 != 0,
                z: extra & 0x02 != 0,
            },
            dpad: DPad {
                up: face & 0x01 != 0,
                right: face & 0x02 != 0,
                down: face & 0x04 != 0,
                left: face & 0x08 != 0,
            },
            left_stick: Stick {
                x: report[17],
                y: report[19],
            },
            right_stick: Stick {
                x: report[21],
                y: report[22],
            },
            left_trigger: report[23],
            right_trigger: report[24],
            gyro: Imu {
                x: i16::from_le_bytes([report[26], report[27]]),
                y: i16::from_le_bytes([report[18], report[20]]),
                z: i16::from_le_bytes([report[29], report[30]]),
            },
            accel: Imu {
                x: i16::from_le_bytes([report[11], report[12]]),
                y: i16::from_le_bytes([report[13], report[14]]),
                z: i16::from_le_bytes([report[15], report[16]]),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NEUTRAL: &[u8] = include_bytes!("../tests/fixtures/vader4-dinput-neutral.bin");
    const PRESSED: &[u8] = include_bytes!("../tests/fixtures/vader4-dinput-pressed.bin");

    #[test]
    fn parses_neutral_fixture() {
        let state = Vader4State::parse(NEUTRAL).unwrap();
        assert_eq!(state.report_id, VADER4_DINPUT_REPORT_ID);
        assert_eq!(state.buttons, Buttons::default());
        assert_eq!(state.dpad, DPad::default());
        assert_eq!(state.left_stick, Stick { x: 128, y: 128 });
        assert_eq!(state.right_stick, Stick { x: 128, y: 128 });
        assert_eq!(state.left_trigger, 0);
        assert_eq!(state.right_trigger, 0);
        assert_eq!(state.gyro, Imu::default());
        assert_eq!(state.accel, Imu::default());
    }

    #[test]
    fn parses_pressed_fixture() {
        let state = Vader4State::parse(PRESSED).unwrap();
        let buttons = state.buttons;
        assert!(buttons.a && buttons.b && buttons.x && buttons.y);
        assert!(buttons.lb && buttons.rb && buttons.lt && buttons.rt);
        assert!(buttons.ls && buttons.rs && buttons.select && buttons.start);
        assert!(buttons.guide && buttons.r#fn);
        assert!(buttons.m1 && buttons.m2 && buttons.m3 && buttons.m4);
        assert!(buttons.c && buttons.z);

        assert_eq!(
            state.dpad,
            DPad {
                up: true,
                right: true,
                down: false,
                left: false,
            }
        );
        assert_eq!(state.left_stick, Stick { x: 0, y: 255 });
        assert_eq!(state.right_stick, Stick { x: 64, y: 192 });
        assert_eq!(state.left_trigger, 128);
        assert_eq!(state.right_trigger, 255);
        assert_eq!(
            state.gyro,
            Imu {
                x: 0x1234,
                y: -17254,
                z: 0x5678,
            }
        );
        assert_eq!(
            state.accel,
            Imu {
                x: 0x0201,
                y: 0x0403,
                z: 0x0605,
            }
        );
    }

    #[test]
    fn rejects_wrong_length_and_report_id() {
        assert_eq!(
            Vader4State::parse(&[0u8; 31]).unwrap_err(),
            ParseError::Length { got: 31 }
        );
        let mut wrong_id = [0u8; VADER4_DINPUT_REPORT_LEN];
        wrong_id[0] = 0x05;
        assert_eq!(
            Vader4State::parse(&wrong_id).unwrap_err(),
            ParseError::ReportId { got: 0x05 }
        );
    }
}
