// socketcan/src/errors.rs
//
// Implements errors for Rust SocketCAN library on Linux.
//
// This file is part of the Rust 'socketcan-rs' library.
//
// Licensed under the MIT license:
//   <LICENSE or http://opensource.org/licenses/MIT>
// This file may not be copied, modified, or distributed except according
// to those terms.

//! CAN bus errors.
//!
//! Most information about the errors on the CANbus are determined from an
//! error frame. To receive them, the error mask must be set on the socket
//! for the types of errors that the application would like to receive.
//!
//! See [RAW Socket Option CAN_RAW_ERR_FILTER](https://docs.kernel.org/networking/can.html#raw-socket-option-can-raw-err-filter)
//!
//! The general types of errors are encoded in the error bits of the CAN ID
//! of an error frame. This is reported with [`CanError`]. Specific errors
//! might indicate that more information can be obtained from the data bytes
//! in the error frame.
//!
//! ```text
//! Lost Arbitration   (0x02) => data[0]
//! Controller Problem (0x04) => data[1]
//! Protocol Violation (0x08) => data[2..3]
//! Transceiver Status (0x10) => data[4]
//!
//! Error Counters (0x200) =>
//!   TX Error Counter => data[6]
//!   RX Error Counter => data[7]
//! ```
//!
//! All of this error information is not well documented, but can be extracted
//! from the Linux kernel header file:
//! [linux/can/error.h](https://raw.githubusercontent.com/torvalds/linux/master/include/uapi/linux/can/error.h)
//!

use crate::{CanErrorFrame, EmbeddedFrame, Frame};
use std::{convert::TryFrom, error, fmt, io};
use thiserror::Error;

// ===== Composite Error for the crate =====

/// Composite SocketCAN error.
///
/// This can be any of the underlying errors from this library.
#[derive(Error, Debug)]
pub enum Error {
    /// A CANbus error, usually from an error frmae
    #[error(transparent)]
    Can(#[from] CanError),
    /// A problem with the CAN controller
    #[error(transparent)]
    Controller(#[from] ControllerProblem),
    /// An error opening the CAN socket
    #[error(transparent)]
    SocketOpen(#[from] CanSocketOpenError),
    /// A lower-level error from the nix library
    #[error(transparent)]
    Nix(#[from] nix::Error),
    /// A low-level I/O error
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl embedded_can::Error for Error {
    fn kind(&self) -> embedded_can::ErrorKind {
        match *self {
            Error::Can(err) => err.kind(),
            _ => embedded_can::ErrorKind::Other,
        }
    }
}

/// A result that can derive from any of the CAN errors.
pub type Result<T> = std::result::Result<T, Error>;

// ===== CanError ====

/// A CAN bus error derived from an error frame.
///
/// An CAN interface device driver can send detailed error information up
/// to the application in an "error frame". These are selectable by the
/// application by applying an error bitmask to the socket to choose which
/// types of errors to receive.
///
/// The error frame can then be converted into this `CanError` which is a
/// proper Rust error type which implements std::error::Error.
///
/// Most error types here corresponds to a bit in the error mask of a CAN ID
/// word of an error frame - a frame in which the CAN error flag
/// (`CAN_ERR_FLAG`) is set. But there are additional types to handle any
/// problems decoding the error frame.
#[derive(Copy, Clone, Debug)]
pub enum CanError {
    /// TX timeout (by netdevice driver)
    TransmitTimeout,
    /// Arbitration was lost.
    /// Contains the bit number after which arbitration was lost or 0 if unspecified.
    LostArbitration(u8),
    /// Controller problem
    ControllerProblem(ControllerProblem),
    /// Protocol violation at the specified [`Location`].
    ProtocolViolation {
        /// The type of protocol violation
        vtype: ViolationType,
        /// The location (field or bit) of the violation
        location: Location,
    },
    /// Transceiver Error.
    TransceiverError,
    /// No ACK received for current CAN frame.
    NoAck,
    /// Bus off (due to too many detected errors)
    BusOff,
    /// Bus error (due to too many detected errors)
    BusError,
    /// The bus has been restarted
    Restarted,
    /// There was an error deciding the error frame
    DecodingFailure(CanErrorDecodingFailure),
    /// Unknown, possibly invalid, error
    Unknown(u32),
}

impl error::Error for CanError {}

impl fmt::Display for CanError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use CanError::*;
        match *self {
            TransmitTimeout => write!(f, "transmission timeout"),
            LostArbitration(n) => write!(f, "arbitration lost after {} bits", n),
            ControllerProblem(e) => write!(f, "controller problem: {}", e),
            ProtocolViolation { vtype, location } => {
                write!(f, "protocol violation at {}: {}", location, vtype)
            }
            TransceiverError => write!(f, "transceiver error"),
            NoAck => write!(f, "no ack"),
            BusOff => write!(f, "bus off"),
            BusError => write!(f, "bus error"),
            Restarted => write!(f, "restarted"),
            DecodingFailure(err) => write!(f, "decoding failure: {}", err),
            Unknown(err) => write!(f, "unknown error ({})", err),
        }
    }
}

impl embedded_can::Error for CanError {
    fn kind(&self) -> embedded_can::ErrorKind {
        match *self {
            CanError::ControllerProblem(cp) => {
                use ControllerProblem::*;
                match cp {
                    ReceiveBufferOverflow | TransmitBufferOverflow => {
                        embedded_can::ErrorKind::Overrun
                    }
                    _ => embedded_can::ErrorKind::Other,
                }
            }
            CanError::NoAck => embedded_can::ErrorKind::Acknowledge,
            _ => embedded_can::ErrorKind::Other,
        }
    }
}

impl From<CanErrorFrame> for CanError {
    /// Constructs a CAN error from an error frame.
    fn from(frame: CanErrorFrame) -> Self {
        // Note that the CanErrorFrame is guaranteed to have the full 8-byte
        // data payload.
        match frame.error_bits() {
            0x0001 => CanError::TransmitTimeout,
            0x0002 => CanError::LostArbitration(frame.data()[0]),
            0x0004 => match ControllerProblem::try_from(frame.data()[1]) {
                Ok(err) => CanError::ControllerProblem(err),
                Err(err) => CanError::DecodingFailure(err),
            },
            0x0008 => {
                match (
                    ViolationType::try_from(frame.data()[2]),
                    Location::try_from(frame.data()[3]),
                ) {
                    (Ok(vtype), Ok(location)) => CanError::ProtocolViolation { vtype, location },
                    (Err(err), _) | (_, Err(err)) => CanError::DecodingFailure(err),
                }
            }
            0x0010 => CanError::TransceiverError,
            0x0020 => CanError::NoAck,
            0x0040 => CanError::BusOff,
            0x0080 => CanError::BusError,
            0x0100 => CanError::Restarted,
            err => CanError::Unknown(err),
        }
    }
}

// ===== ControllerProblem =====

/// Error status of the CAN conroller.
///
/// This is derived from `data[1]` of an error frame
#[derive(Copy, Clone, Debug)]
pub enum ControllerProblem {
    /// unspecified
    Unspecified,
    /// RX buffer overflow
    ReceiveBufferOverflow,
    /// TX buffer overflow
    TransmitBufferOverflow,
    /// reached warning level for RX errors
    ReceiveErrorWarning,
    /// reached warning level for TX errors
    TransmitErrorWarning,
    /// reached error passive status RX
    ReceiveErrorPassive,
    /// reached error passive status TX
    TransmitErrorPassive,
    /// recovered to error active state
    Active,
}

impl error::Error for ControllerProblem {}

impl fmt::Display for ControllerProblem {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use ControllerProblem::*;
        let msg = match *self {
            Unspecified => "unspecified controller problem",
            ReceiveBufferOverflow => "receive buffer overflow",
            TransmitBufferOverflow => "transmit buffer overflow",
            ReceiveErrorWarning => "ERROR WARNING (receive)",
            TransmitErrorWarning => "ERROR WARNING (transmit)",
            ReceiveErrorPassive => "ERROR PASSIVE (receive)",
            TransmitErrorPassive => "ERROR PASSIVE (transmit)",
            Active => "ERROR ACTIVE",
        };
        write!(f, "{}", msg)
    }
}

impl TryFrom<u8> for ControllerProblem {
    type Error = CanErrorDecodingFailure;

    fn try_from(val: u8) -> std::result::Result<Self, Self::Error> {
        use ControllerProblem::*;
        Ok(match val {
            0x00 => Unspecified,
            0x01 => ReceiveBufferOverflow,
            0x02 => TransmitBufferOverflow,
            0x04 => ReceiveErrorWarning,
            0x08 => TransmitErrorWarning,
            0x10 => ReceiveErrorPassive,
            0x20 => TransmitErrorPassive,
            0x40 => Active,
            _ => return Err(CanErrorDecodingFailure::InvalidControllerProblem),
        })
    }
}

// ===== ViolationType =====

/// The type of protocol violation error.
///
/// This is derived from `data[2]` of an error frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ViolationType {
    /// Unspecified Violation
    Unspecified = 0x00,
    /// Single Bit Error
    SingleBitError = 0x01,
    /// Frame formatting error
    FrameFormatError = 0x02,
    /// Bit stuffing error
    BitStuffingError = 0x04,
    /// A dominant bit was sent, but not received
    UnableToSendDominantBit = 0x08,
    /// A recessive bit was sent, but not received
    UnableToSendRecessiveBit = 0x10,
    /// Bus overloaded
    BusOverload = 0x20,
    /// Bus is active (again)
    Active = 0x40,
    /// Transmission Error
    TransmissionError = 0x80,
}

impl error::Error for ViolationType {}

impl fmt::Display for ViolationType {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use ViolationType::*;
        let msg = match *self {
            Unspecified => "unspecified",
            SingleBitError => "single bit error",
            FrameFormatError => "frame format error",
            BitStuffingError => "bit stuffing error",
            UnableToSendDominantBit => "unable to send dominant bit",
            UnableToSendRecessiveBit => "unable to send recessive bit",
            BusOverload => "bus overload",
            Active => "active",
            TransmissionError => "transmission error",
        };
        write!(f, "{}", msg)
    }
}

impl TryFrom<u8> for ViolationType {
    type Error = CanErrorDecodingFailure;

    fn try_from(val: u8) -> std::result::Result<Self, Self::Error> {
        use ViolationType::*;
        Ok(match val {
            0x00 => Unspecified,
            0x01 => SingleBitError,
            0x02 => FrameFormatError,
            0x04 => BitStuffingError,
            0x08 => UnableToSendDominantBit,
            0x10 => UnableToSendRecessiveBit,
            0x20 => BusOverload,
            0x40 => Active,
            0x80 => TransmissionError,
            _ => return Err(CanErrorDecodingFailure::InvalidViolationType),
        })
    }
}

/// The location of a CANbus protocol violation.
///
/// This describes the position inside a received frame (as in the field
/// or bit) at which an error occured.
///
/// This is derived from `data[1]` of an error frame.
#[derive(Copy, Clone, Debug)]
pub enum Location {
    /// Unspecified
    Unspecified,
    /// Start of frame.
    StartOfFrame,
    /// ID bits 28-21 (SFF: 10-3)
    Id2821,
    /// ID bits 20-18 (SFF: 2-0)
    Id2018,
    /// substitute RTR (SFF: RTR)
    SubstituteRtr,
    /// extension of identifier
    IdentifierExtension,
    /// ID bits 17-13
    Id1713,
    /// ID bits 12-5
    Id1205,
    /// ID bits 4-0
    Id0400,
    /// RTR bit
    Rtr,
    /// Reserved bit 1
    Reserved1,
    /// Reserved bit 0
    Reserved0,
    /// Data length
    DataLengthCode,
    /// Data section
    DataSection,
    /// CRC sequence
    CrcSequence,
    /// CRC delimiter
    CrcDelimiter,
    /// ACK slot
    AckSlot,
    /// ACK delimiter
    AckDelimiter,
    /// End-of-frame
    EndOfFrame,
    /// Intermission (between frames)
    Intermission,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use Location::*;
        let msg = match *self {
            Unspecified => "unspecified location",
            StartOfFrame => "start of frame",
            Id2821 => "ID, bits 28-21",
            Id2018 => "ID, bits 20-18",
            SubstituteRtr => "substitute RTR bit",
            IdentifierExtension => "ID, extension",
            Id1713 => "ID, bits 17-13",
            Id1205 => "ID, bits 12-05",
            Id0400 => "ID, bits 04-00",
            Rtr => "RTR bit",
            Reserved1 => "reserved bit 1",
            Reserved0 => "reserved bit 0",
            DataLengthCode => "data length code",
            DataSection => "data section",
            CrcSequence => "CRC sequence",
            CrcDelimiter => "CRC delimiter",
            AckSlot => "ACK slot",
            AckDelimiter => "ACK delimiter",
            EndOfFrame => "end of frame",
            Intermission => "intermission",
        };
        write!(f, "{}", msg)
    }
}
impl TryFrom<u8> for Location {
    type Error = CanErrorDecodingFailure;

    fn try_from(val: u8) -> std::result::Result<Self, Self::Error> {
        use Location::*;
        Ok(match val {
            0x00 => Unspecified,
            0x03 => StartOfFrame,
            0x02 => Id2821,
            0x06 => Id2018,
            0x04 => SubstituteRtr,
            0x05 => IdentifierExtension,
            0x07 => Id1713,
            0x0F => Id1205,
            0x0E => Id0400,
            0x0C => Rtr,
            0x0D => Reserved1,
            0x09 => Reserved0,
            0x0B => DataLengthCode,
            0x0A => DataSection,
            0x08 => CrcSequence,
            0x18 => CrcDelimiter,
            0x19 => AckSlot,
            0x1B => AckDelimiter,
            0x1A => EndOfFrame,
            0x12 => Intermission,
            _ => return Err(CanErrorDecodingFailure::InvalidLocation),
        })
    }
}

// ===== TransceiverError =====

/// The error status of the CAN transceiver.
///
/// This is derived from `data[4]` of an error frame.
#[derive(Copy, Clone, Debug)]
pub enum TransceiverError {
    /// Unsecified
    Unspecified,
    /// CAN High, no wire
    CanHighNoWire,
    /// CAN High, short to BAT
    CanHighShortToBat,
    /// CAN High, short to VCC
    CanHighShortToVcc,
    /// CAN High, short to GND
    CanHighShortToGnd,
    /// CAN Low, no wire
    CanLowNoWire,
    /// CAN Low, short to BAT
    CanLowShortToBat,
    /// CAN Low, short to VCC
    CanLowShortToVcc,
    /// CAN Low, short to GND
    CanLowShortToGnd,
    /// CAN Low short to  CAN High
    CanLowShortToCanHigh,
}

impl TryFrom<u8> for TransceiverError {
    type Error = CanErrorDecodingFailure;

    fn try_from(val: u8) -> std::result::Result<Self, Self::Error> {
        use TransceiverError::*;
        Ok(match val {
            0x00 => Unspecified,
            0x04 => CanHighNoWire,
            0x05 => CanHighShortToBat,
            0x06 => CanHighShortToVcc,
            0x07 => CanHighShortToGnd,
            0x40 => CanLowNoWire,
            0x50 => CanLowShortToBat,
            0x60 => CanLowShortToVcc,
            0x70 => CanLowShortToGnd,
            0x80 => CanLowShortToCanHigh,
            _ => return Err(CanErrorDecodingFailure::InvalidTransceiverError),
        })
    }
}

/// Get the controller specific error information.
pub trait ControllerSpecificErrorInformation {
    /// Get the controller specific error information.
    fn get_ctrl_err(&self) -> Option<&[u8]>;
}

impl<T: Frame> ControllerSpecificErrorInformation for T {
    /// Get the controller specific error information.
    fn get_ctrl_err(&self) -> Option<&[u8]> {
        let data = self.data();

        if data.len() == 8 {
            Some(&data[5..])
        } else {
            None
        }
    }
}

// ===== CanErrorDecodingFailure =====

/// Error decoding a CanError from a CanErrorFrame.
#[derive(Copy, Clone, Debug)]
pub enum CanErrorDecodingFailure {
    /// The supplied CANFrame did not have the error bit set.
    NotAnError,
    /// The error type is not known and cannot be decoded.
    UnknownErrorType(u32),
    /// The error type indicated a need for additional information as `data`,
    /// but the `data` field was not long enough.
    NotEnoughData(u8),
    /// The error type `ControllerProblem` was indicated and additional
    /// information found, but not recognized.
    InvalidControllerProblem,
    /// The type of the ProtocolViolation was not valid
    InvalidViolationType,
    /// A location was specified for a ProtocolViolation, but the location
    /// was not valid.
    InvalidLocation,
    /// The supplied transciever error was invalid.
    InvalidTransceiverError,
}

impl error::Error for CanErrorDecodingFailure {}

impl fmt::Display for CanErrorDecodingFailure {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use CanErrorDecodingFailure::*;
        let msg = match *self {
            NotAnError => "CAN frame is not an error",
            UnknownErrorType(_) => "unknown error type",
            NotEnoughData(_) => "not enough data",
            InvalidControllerProblem => "not a valid controller problem",
            InvalidViolationType => "not a valid violation type",
            InvalidLocation => "not a valid location",
            InvalidTransceiverError => "not a valid transceiver error",
        };
        write!(f, "{}", msg)
    }
}

// ===== CanSocketOpenError =====

#[derive(Debug)]
/// Errors opening socket
pub enum CanSocketOpenError {
    /// Device could not be found
    LookupError(nix::Error),

    /// System error while trying to look up device name
    IOError(io::Error),
}

impl error::Error for CanSocketOpenError {}

impl fmt::Display for CanSocketOpenError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use CanSocketOpenError::*;
        match *self {
            LookupError(ref e) => write!(f, "CAN Device not found: {}", e),
            IOError(ref e) => write!(f, "IO: {}", e),
        }
    }
}

impl From<nix::Error> for CanSocketOpenError {
    fn from(e: nix::Error) -> Self {
        Self::LookupError(e)
    }
}

impl From<io::Error> for CanSocketOpenError {
    fn from(e: io::Error) -> Self {
        Self::IOError(e)
    }
}

// ===== ConstructionError =====

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
/// Error that occurs when creating CAN packets
pub enum ConstructionError {
    /// Trying to create a specific frame type from an incompatible type
    WrongFrameType,
    /// CAN ID was outside the range of valid IDs
    IDTooLarge,
    /// Larger payload reported than can be held in the frame.
    TooMuchData,
}

impl error::Error for ConstructionError {}

impl fmt::Display for ConstructionError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use ConstructionError::*;
        match *self {
            WrongFrameType => write!(f, "Incompatible frame type"),
            IDTooLarge => write!(f, "CAN ID too large"),
            TooMuchData => write!(f, "Payload is too large"),
        }
    }
}
