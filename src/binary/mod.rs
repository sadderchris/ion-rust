// Copyright Amazon.com, Inc. or its affiliates.

//! This module provides the necessary structures and logic to read values from a binary Ion
//! data stream.

pub(crate) mod constants;
pub mod decimal;
mod header;
mod int;
mod nibbles;
pub(crate) mod raw_binary_reader;
pub mod timestamp;
mod type_code;
pub mod uint;
mod var_int;
mod var_uint;
pub mod writer;

pub use type_code::IonTypeCode;
