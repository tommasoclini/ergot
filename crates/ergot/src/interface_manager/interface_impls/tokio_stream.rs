//! Generic tokio stream interface impl
//!
//! Uses COBS for framing over any tokio AsyncRead/AsyncWrite stream.

use crate::interface_manager::{
    Interface,
    utils::{cobs_stream, std::StdQueue},
};

/// An interface implementation for generic tokio async streams
pub struct TokioStreamInterface {}

impl Interface for TokioStreamInterface {
    type Sink = cobs_stream::Sink<StdQueue>;
}
