use std::convert::TryFrom;
use bigdecimal::BigDecimal;
use chrono::{DateTime, FixedOffset};
use nom::Err::Incomplete;
use nom::IResult;

use crate::result::{decoding_error, illegal_operation, IonResult};
use crate::text::parent_level::ParentContainer;
use crate::text::parsers::containers::{
    list_value_or_end, s_expression_value_or_end, struct_field_name_or_end, struct_field_value,
};
use crate::text::parsers::top_level::top_level_value;
use crate::text::text_buffer::TextBuffer;
use crate::text::text_data_source::TextIonDataSource;
use crate::text::text_value::{AnnotatedTextValue, TextValue};
use crate::value::owned::OwnedSymbolToken;
use crate::{SystemReader, IonType};
use crate::system_reader::StreamItem;
use crate::types::decimal::Decimal;
use crate::types::SymbolId;
use crate::types::timestamp::Timestamp;

// TODO: Implement a full text reader.
//       This implementation is a placeholder. It does not yet implement the Cursor trait.

const INITIAL_PARENTS_CAPACITY: usize = 16;

pub struct TextReader<T: TextIonDataSource> {
    buffer: TextBuffer<T::TextSource>,
    current_field_name: Option<OwnedSymbolToken>,
    current_value: Option<AnnotatedTextValue>,
    bytes_read: usize,
    is_eof: bool,
    parents: Vec<ParentContainer>,
}

impl<T: TextIonDataSource> TextReader<T> {
    fn new(input: T) -> TextReader<T> {
        let text_source = input.to_text_ion_data_source();
        TextReader {
            buffer: TextBuffer::new(text_source),
            current_field_name: None,
            current_value: None,
            bytes_read: 0,
            is_eof: false,
            parents: Vec::with_capacity(INITIAL_PARENTS_CAPACITY),
        }
    }

    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }

    fn load_next_value(&mut self) -> IonResult<()> {
        // If the reader's current value is the beginning of a container and the user calls `next()`,
        // We need to skip the entire container. We can do this by stepping into and then out of
        // that container. `step_out()` has logic that will exhaust the remaining values.
        let need_to_skip_container = self
            .current_value
            .as_ref()
            .map(|v| v.value().ion_type().is_container())
            .unwrap_or(false);

        if need_to_skip_container {
            self.step_in()?;
            self.step_out()?;
        }

        if self.parents.is_empty() {
            // The `parents` stack is empty. We're at the top level.

            // If the reader has already found EOF (the end of the top level), there's no need to
            // try to read more data. Return Ok(None).
            if self.is_eof {
                self.current_value = None;
                return Ok(());
            }
            // Otherwise, try to read the next value.
            let value = self.next_top_level_value();
            match value {
                Ok(None) => {
                    // We hit EOF; make a note of it and clear the current value.
                    self.is_eof = true;
                    self.current_value = None;
                }
                Ok(Some(ref value)) => {
                    // We read a value successfully; set it as our current value.
                    // TODO: This currently clones the loaded value. This will not be necessary
                    //       when `next()` returns an IonType instead of an AnnotatedTextValue.
                    self.current_value = Some(value.clone());
                }
                _ => {}
            };
            return Ok(());
        }
        // Otherwise, the `parents` stack is not empty. We're inside a container.

        // The `ParentLevel` type is only a couple of stack-allocated bytes. It's very cheap to clone.
        let parent = self.parents.last().unwrap().clone();
        // If the reader had already found the end of this container, return Ok(None).
        if parent.is_exhausted() {
            self.current_value = None;
            return Ok(());
        }
        // Otherwise, try to read the next value. The syntax we expect will depend on the
        // IonType of the parent container.
        let value = match parent.ion_type() {
            IonType::List => self.next_list_value(),
            IonType::SExpression => self.next_s_expression_value(),
            IonType::Struct => {
                // If the reader finds a field name...
                if let Some(field_name) = self.next_struct_field_name()? {
                    // ...remember it and return the field value that follows.
                    self.current_field_name = Some(field_name);
                    Ok(Some(self.next_struct_field_value()?))
                } else {
                    // Otherwise, this is the end of the struct.
                    Ok(None)
                }
            }
            other => unreachable!(
                "The reader's `parents` stack contained a scalar value: {:?}",
                other
            ),
        };

        match value {
            Ok(None) => {
                // If the parser returns Ok(None), we've just encountered the end of the container for
                // the first time. Set `is_exhausted` so we won't try to parse more until `step_out()` is
                // called.
                // We previously used a copy of the last `ParentLevel` in the stack to simplify reading.
                // To modify it, we'll need to get a mutable reference to the original.
                self.parents.last_mut().unwrap().set_exhausted(true);
                self.current_value = None;
            },
            Ok(Some(value)) => {
                // We successfully read a value. Set it as the current value.
                self.current_value = Some(value);
            },
            Err(e) => return Err(e)
        };

        Ok(())
    }

    fn field_name(&self) -> Option<&OwnedSymbolToken> {
        self.current_field_name.as_ref()
    }

    /// Assumes that the reader is at the top level and attempts to parse the next value or IVM in
    /// the stream.
    fn next_top_level_value(&mut self) -> IonResult<Option<AnnotatedTextValue>> {
        match self.parse_next(top_level_value) {
            Ok(Some(value)) => Ok(Some(value)),
            Ok(None) => {
                // The top level is the only depth at which EOF is legal. If we encounter an EOF,
                // double check that the buffer doesn't actually have a value in it. See the
                // comments in [parse_value_at_eof] for a detailed explanation of this.
                self.parse_value_at_eof()
            }
            Err(e) => Err(e),
        }
    }

    /// Assumes that the reader is inside a list and attempts to parse the next value.
    /// If the next token in the stream is an end-of-list delimiter (`]`), returns Ok(None).
    fn next_list_value(&mut self) -> IonResult<Option<AnnotatedTextValue>> {
        self.parse_expected("a list", list_value_or_end)
    }

    /// Assumes that the reader is inside an s-expression and attempts to parse the next value.
    /// If the next token in the stream is an end-of-s-expression delimiter (`)`), returns Ok(None).
    fn next_s_expression_value(&mut self) -> IonResult<Option<AnnotatedTextValue>> {
        self.parse_expected("an s-expression", s_expression_value_or_end)
    }

    /// Assumes that the reader is inside an struct and attempts to parse the next field name.
    /// If the next token in the stream is an end-of-struct delimiter (`}`), returns Ok(None).
    fn next_struct_field_name(&mut self) -> IonResult<Option<OwnedSymbolToken>> {
        // If there isn't another value, this returns Ok(None).
        self.parse_expected("a struct field name", struct_field_name_or_end)
    }

    /// Assumes that the reader is inside a struct AND that a field has already been successfully
    /// parsed from input using [next_struct_field_name] and attempts to parse the next value.
    /// In this input position, only a value (or whitespace/comments) are legal. Anything else
    /// (including EOF) will result in a decoding error.
    fn next_struct_field_value(&mut self) -> IonResult<AnnotatedTextValue> {
        // Only called after a call to [next_struct_field_name] that returns Some(field_name).
        // It is not legal for a field name to be followed by a '}' or EOF.
        // If there isn't another value, returns an Err.
        self.parse_expected("a struct field value", struct_field_value)
    }

    /// Attempts to parse the next entity from the stream using the provided parser.
    /// Returns a decoding error if EOF is encountered while parsing.
    /// If the parser encounters an error, it will be returned as-is.
    fn parse_expected<P, O>(&mut self, entity_name: &str, parser: P) -> IonResult<O>
    where
        P: Fn(&str) -> IResult<&str, O>,
    {
        match self.parse_next(parser) {
            Ok(Some(value)) => Ok(value),
            Ok(None) => decoding_error(format!(
                "Unexpected end of input while reading {} on line {}: '{}'",
                entity_name,
                self.buffer.lines_loaded(),
                self.buffer.remaining_text()
            )),
            Err(e) => Err(e),
        }
    }

    /// Attempts to parse the next entity from the stream using the provided parser.
    /// If there isn't enough data in the buffer for the parser to match its input conclusively,
    /// more data will be loaded into the buffer and the parser will be called again.
    /// If EOF is encountered, returns `Ok(None)`.
    fn parse_next<P, O>(&mut self, parser: P) -> IonResult<Option<O>>
    where
        P: Fn(&str) -> IResult<&str, O>,
    {
        if self.is_eof {
            return Ok(None);
        }

        let value = 'parse: loop {
            // Note the number of bytes currently in the text buffer
            let length_before_parse = self.buffer.remaining_text().len();
            // Invoke the top_level_value() parser; this will attempt to recognize the next value
            // in the stream and return a &str slice containing the remaining, not-yet-parsed text.
            match parser(self.buffer.remaining_text()) {
                // If `top_level_value` returns 'Incomplete', there wasn't enough text in the buffer
                // to match the next value. No syntax errors have been encountered (yet?), but we
                // need to load more text into the buffer before we try to parse it again.
                Err(Incomplete(_needed)) => {
                    // Ask the buffer to load another line of text.
                    // TODO: Currently this loads a single line at a time for easier testing.
                    //       We may wish to bump it to a higher number of lines at a time (8?)
                    //       for efficiency once we're confident in the correctness.
                    if self.buffer.load_next_line()? == 0 {
                        // If load_next_line() returns Ok(0), we've reached the end of our input.
                        self.is_eof = true;
                        // The buffer had an `Incomplete` value in it; now that we know we're at EOF,
                        // we can determine whether the buffer's contents should actually be
                        // considered complete.
                        return Ok(None);
                    }
                    continue;
                }
                Ok((remaining_text, value)) => {
                    // Our parser successfully matched a value.
                    // Note the length of the text that remains after parsing.
                    let length_after_parse = remaining_text.len();
                    // The difference in length tells us how many bytes were part of the
                    // text representation of the value that we found.
                    let bytes_consumed = length_before_parse - length_after_parse;
                    // Discard `bytes_consumed` bytes from the TextBuffer.
                    self.buffer.consume(bytes_consumed);
                    self.bytes_read += bytes_consumed;
                    // Break out of the read/parse loop, returning the value that we matched.
                    break 'parse value;
                }
                Err(e) => {
                    // Return an error that contains the text currently in the buffer (i.e. what we
                    // were attempting to parse with `top_level_value`.)
                    // TODO: We probably don't want to surface the nom error (`e`) directly, but it's
                    //       useful for debugging.
                    return decoding_error(format!(
                        "Parsing error occurred near line {}: '{}': '{}'",
                        self.buffer.lines_loaded(),
                        self.buffer.remaining_text(),
                        e
                    ));
                }
            };
        };

        Ok(Some(value))
    }

    // Parses the contents of the text buffer again with the knowledge that we're at the end of the
    // input stream. This allows us to resolve a number of ambiguous cases.
    // For a detailed description of the problem that this addresses, please see:
    // https://github.com/amzn/ion-rust/issues/318
    // This method should only be called when the reader is at the top level. An EOF at any other
    // depth is an error.
    fn parse_value_at_eof(&mut self) -> IonResult<Option<AnnotatedTextValue>> {
        // An arbitrary, cheap-to-parse Ion value that we append to the buffer when its contents at
        // EOF are ambiguous.
        const SENTINEL_ION_TEXT: &str = "\n0\n";
        // Make a note of the buffer's length; we're about to modify it.
        let original_length = self.buffer.remaining_text().len();
        // Append our sentinel value to the end of the input buffer.
        self.buffer.inner().push_str(SENTINEL_ION_TEXT);
        // If the buffer contained a value, the newline will indicate that the contents of the
        // buffer were complete. For example:
        // * the integer `7` becomes `7\n`; it wasn't the first digit in a truncated `755`.
        // * the boolean `true` becomes `false\n`; it wasn't actually half of the
        //   identifier `falseTeeth`.
        //
        // If the buffer contained a value that's written in segments, the extra `0` will indicate
        // that no more segments are coming. For example:
        // * `foo::bar` becomes `foo::bar\n0\n`; the parser can see that 'bar' is a value, not
        //    another annotation in the sequence.
        // * `'''long-form string'''` becomes `'''long-form string'''\n0\n`; the parser can see that
        //   there aren't any more long-form string segments in the sequence.
        //
        // Attempt to parse the updated buffer.
        let value = match top_level_value(self.buffer.remaining_text()) {
            Ok(("\n", value))
                if value.annotations().len() == 0 && *value.value() == TextValue::Integer(0) =>
            {
                // We found the unannotated zero that we appended to the end of the buffer.
                // The "\n" in this pattern is the unparsed text left in the buffer,
                // which indicates that our 0 was parsed.
                Ok(None)
            }
            Ok((_remaining_text, value)) => {
                // We found something else. The zero is still in the buffer; we can leave it there.
                // The reader's `is_eof` flag has been set, so the text buffer will never be used
                // again. Return the value we found.
                Ok(Some(value))
            }
            Err(Incomplete(_needed)) => {
                decoding_error(format!(
                    "Unexpected end of input on line {}: '{}'",
                    self.buffer.lines_loaded(),
                    &self.buffer.remaining_text()[..original_length] // Don't show the extra `\n0\n`
                ))
            }
            Err(e) => {
                decoding_error(format!(
                    "Parsing error occurred near line {}: '{}': '{}'",
                    self.buffer.lines_loaded(),
                    &self.buffer.remaining_text()[..original_length], // Don't show the extra `\n0\n`
                    e
                ))
            }
        };

        // If we didn't consume the sentinel value, remove the sentinel value from the buffer.
        // Doing so makes this method idempotent.
        if self.buffer.remaining_text().ends_with(SENTINEL_ION_TEXT) {
            let length = self.buffer.remaining_text().len();
            self.buffer
                .inner()
                .truncate(length - SENTINEL_ION_TEXT.len());
        }

        value
    }
}

impl <T: TextIonDataSource> SystemReader for TextReader<T> {

    fn ion_version(&self) -> (u8, u8) {
        // TODO: The text reader does not yet have IVM support
        (1, 0)
    }

    fn next(&mut self) -> IonResult<Option<StreamItem>> {
        // Parse the next value from the stream, storing it in `self.current_value`.
        self.load_next_value()?;
        if let Some(value) = self.current_value.as_ref() {
            let ion_type = value.ion_type();
            let is_null = matches!(value.value(), TextValue::Null(_));
            Ok(Some(StreamItem::Value(ion_type, is_null)))
        } else {
            Ok(None)
        }
    }

    fn ion_type(&self) -> Option<IonType> {
        self.current_value
            .as_ref()
            .map(|v| v.ion_type())
    }

    fn is_null(&self) -> bool {
        self.current_value
            .as_ref()
            .map(|v| matches!(v.value(), TextValue::Null(_)))
            .unwrap_or(false)
    }

    fn annotation_ids(&self) -> &[SymbolId] {
        //TODO: Update trait to use `OwnedSymbolToken`
        todo!()
    }

    fn field_id(&self) -> Option<SymbolId> {
        //TODO: Update trait to use `OwnedSymbolToken`
        todo!()
    }

    fn read_null(&mut self) -> IonResult<Option<IonType>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::Null(ion_type)) => Ok(Some(*ion_type)),
            _ => Ok(None)
        }
    }

    fn read_bool(&mut self) -> IonResult<Option<bool>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::Boolean(value)) => Ok(Some(*value)),
            _ => Ok(None)
        }
    }

    fn read_i64(&mut self) -> IonResult<Option<i64>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::Integer(value)) => Ok(Some(*value)),
            _ => Ok(None)
        }
    }

    fn read_f32(&mut self) -> IonResult<Option<f32>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::Float(value)) => Ok(Some(*value as f32)),
            _ => Ok(None)
        }
    }

    fn read_f64(&mut self) -> IonResult<Option<f64>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::Float(value)) => Ok(Some(*value)),
            _ => Ok(None)
        }
    }

    fn read_decimal(&mut self) -> IonResult<Option<Decimal>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::Decimal(ref value)) => Ok(Some(value.clone())),
            _ => Ok(None)
        }
    }

    fn read_big_decimal(&mut self) -> IonResult<Option<BigDecimal>> {
        // TODO: This function is deprecated. Remove it from the trait.
        todo!()
    }

    fn read_string(&mut self) -> IonResult<Option<String>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::String(ref value)) => Ok(Some(value.clone())),
            _ => Ok(None)
        }
    }

    fn string_ref_map<F, U>(&mut self, f: F) -> IonResult<Option<U>> where F: FnOnce(&str) -> U {
        // TODO: This function format is a holdover from pre-NLL Rust. Change to read_str()
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::String(ref value)) => Ok(Some(f(value.as_str()))),
            _ => Ok(None)
        }
    }

    fn string_bytes_map<F, U>(&mut self, f: F) -> IonResult<Option<U>> where F: FnOnce(&[u8]) -> U {
        // TODO: This function format is a holdover from pre-NLL Rust. Change to read_str_bytes()
        todo!()
    }

    fn read_symbol_id(&mut self) -> IonResult<Option<SymbolId>> {
        todo!()
    }

    fn read_blob_bytes(&mut self) -> IonResult<Option<Vec<u8>>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::Blob(ref value)) => Ok(Some(value.clone())),
            _ => Ok(None)
        }
    }

    fn blob_ref_map<F, U>(&mut self, f: F) -> IonResult<Option<U>> where F: FnOnce(&[u8]) -> U {
        // TODO: This function format is a holdover from pre-NLL Rust. Change to read_blob_slice()
        todo!()
    }

    fn read_clob_bytes(&mut self) -> IonResult<Option<Vec<u8>>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::Clob(ref value)) => Ok(Some(value.clone())),
            _ => Ok(None)
        }
    }

    fn clob_ref_map<F, U>(&mut self, f: F) -> IonResult<Option<U>> where F: FnOnce(&[u8]) -> U {
        // TODO: This function format is a holdover from pre-NLL Rust. Change to read_clob_slice()
        todo!()
    }

    fn read_timestamp(&mut self) -> IonResult<Option<Timestamp>> {
        match self.current_value
            .as_ref()
            .map(|current| current.value()) {
            Some(TextValue::Timestamp(ref value)) => Ok(Some(value.clone())),
            _ => Ok(None)
        }
    }

    fn read_datetime(&mut self) -> IonResult<Option<DateTime<FixedOffset>>> {
        // TODO: This is deprecated. Remove it from the trait.
        todo!()
    }

    fn step_in(&mut self) -> IonResult<()> {
        match &self.current_value {
            Some(value) if value.value().ion_type().is_container() => {
                self.parents
                    .push(ParentContainer::new(value.value().ion_type()));
                self.current_value = None;
                Ok(())
            }
            Some(value) => illegal_operation(format!(
                "Cannot step_in() to a {:?}",
                value.value().ion_type()
            )),
            None => illegal_operation(format!(
                "{} {}",
                "Cannot `step_in`: the reader is not positioned on a value.",
                "Try calling `next()` to advance first."
            )),
        }
    }

    fn step_out(&mut self) -> IonResult<()> {
        if self.parents.is_empty() {
            return illegal_operation(format!(
                "Cannot call `step_out()` when the reader is at the top level."
            ));
        }

        // If we're not at the end of the current container, advance the cursor until we are.
        // Unlike the binary reader, which can skip-scan, the text reader must visit every value
        // between its current position and the end of the container.
        if !self.parents.last().unwrap().is_exhausted() {
            while let Some(_ignored_value) = self.next()? {}
        }

        // Remove the parent container from the stack and clear the current value.
        let _ = self.parents.pop();
        self.current_value = None;

        Ok(())
    }

    fn depth(&self) -> usize {
        self.parents.len()
    }
}

#[cfg(test)]
mod reader_tests {
    use rstest::*;

    use crate::result::IonResult;
    use crate::text::reader::TextReader;
    use crate::text::text_value::TextValue;
    use crate::types::decimal::Decimal;
    use crate::types::timestamp::Timestamp;
    use crate::value::owned::{local_sid_token, text_token};
    use crate::{SystemReader, IonType};
    use crate::system_reader::StreamItem;

    fn next_type(reader: &mut TextReader<&str>, ion_type: IonType, is_null: bool) {
        assert_eq!(reader.next().unwrap().unwrap(), StreamItem::Value(ion_type, is_null));
    }

    #[test]
    fn test_skipping_containers() -> IonResult<()> {
        let ion_data = r#"
            0 [1, 2, 3] (4 5) 6
        "#;
        let reader = &mut TextReader::new(ion_data);
        next_type(reader, IonType::Integer, false);
        assert_eq!(reader.read_i64()?.unwrap(), 0);

        next_type(reader, IonType::List, false);
        reader.step_in()?;
        next_type(reader, IonType::Integer, false);
        assert_eq!(reader.read_i64()?.unwrap(), 1);
        reader.step_out()?;
        // This should have skipped over the `2, 3` at the end of the list.
        next_type(reader, IonType::SExpression, false);
        // Don't step into the s-expression. Instead, skip over it.
        next_type(reader, IonType::Integer, false);
        assert_eq!(reader.read_i64()?.unwrap(), 6);
        Ok(())
    }

    #[rstest]
    #[case(" null ", TextValue::Null(IonType::Null))]
    #[case(" null.string ", TextValue::Null(IonType::String))]
    #[case(" true ", TextValue::Boolean(true))]
    #[case(" false ", TextValue::Boolean(false))]
    #[case(" 738 ", TextValue::Integer(738))]
    #[case(" 2.5e0 ", TextValue::Float(2.5))]
    #[case(" 2.5 ", TextValue::Decimal(Decimal::new(25, -1)))]
    #[case(" 2007-07-12T ", TextValue::Timestamp(Timestamp::with_ymd(2007, 7, 12).build().unwrap()))]
    #[case(" foo ", TextValue::Symbol(text_token("foo")))]
    #[case(" \"hi!\" ", TextValue::String("hi!".to_owned()))]
    #[case(" {{ZW5jb2RlZA==}} ", TextValue::Blob(Vec::from("encoded".as_bytes())))]
    #[case(" {{\"hello\"}} ", TextValue::Clob(Vec::from("hello".as_bytes())))]
    fn test_read_single_top_level_values(#[case] text: &str, #[case] expected_value: TextValue) {
        let reader = &mut TextReader::new(text);
        next_type(
            reader,
            expected_value.ion_type(),
            matches!(expected_value, TextValue::Null(_))
        );
        // TODO: Redo (or remove?) this test. There's not an API that exposes the
        //       AnnotatedTextValue any more. We're directly accessing `current_value` as a hack.
        let actual_value = reader.current_value.clone();
        assert_eq!(actual_value.unwrap(), expected_value.without_annotations());
    }

    #[test]
    fn test_text_read_multiple_top_level_values() -> IonResult<()> {
        let ion_data = r#"
            null
            true
            5
            5e0
            5.5
            2021-09-25T
            foo
            "hello"
            {foo: bar}
            ["foo", "bar"]
            ('''foo''')
        "#;

        let reader = &mut TextReader::new(ion_data);
        next_type(reader, IonType::Null, true);

        next_type(reader, IonType::Boolean, false);
        assert_eq!(reader.read_bool()?.unwrap(), true);

        next_type(reader, IonType::Integer, false);
        assert_eq!(reader.read_i64()?.unwrap(), 5);

        next_type(reader, IonType::Float, false);
        assert_eq!(reader.read_f64()?.unwrap(), 5.0f64);

        next_type(reader, IonType::Decimal, false);
        assert_eq!(reader.read_decimal()?.unwrap(), Decimal::new(55, -1));

        next_type(reader, IonType::Timestamp, false);
        assert_eq!(reader.read_timestamp()?.unwrap(), Timestamp::with_ymd(2021, 9, 25).build().unwrap());

        next_type(reader, IonType::Symbol, false);
        // TODO: Read in symbol 'foo' after updating read_symbol to use OwnedSymbolToken
        // assert_eq!(reader.read_symbol_id()?.unwrap(), text_token("foo"));

        next_type(reader, IonType::String, false);
        assert_eq!(reader.read_string()?.unwrap(), "hello".to_string());

        // ===== CONTAINERS =====
        // TODO: Calls to `next()` should return the IonType of the next value, not the value itself.

        // Reading a struct: {foo: bar}
        next_type(reader, IonType::Struct, false);
        reader.step_in();
        next_type(reader, IonType::Symbol, false);
        // TODO: Read in symbol 'bar' after updating read_symbol to use OwnedSymbolToken

        // TODO: Field name ... OwnedSymbolToken
        // assert_eq!(*reader.field_name().unwrap(), text_token("foo"));
        assert_eq!(reader.next()?, None);
        reader.step_out()?;

        // Reading a list: ["foo", "bar"]
        next_type(reader, IonType::List, false);
        reader.step_in()?;
        next_type(reader, IonType::String, false);
        assert_eq!(reader.read_string()?.unwrap(), String::from("foo"));
        next_type(reader, IonType::String, false);
        assert_eq!(reader.read_string()?.unwrap(), String::from("bar"));
        assert_eq!(reader.next()?, None);
        reader.step_out()?;

        // Reading an s-expression: ('''foo''')
        next_type(reader, IonType::SExpression, false);
        reader.step_in()?;
        next_type(reader, IonType::String, false);
        assert_eq!(reader.read_string()?.unwrap(), String::from("foo"));
        assert_eq!(reader.next()?, None);
        reader.step_out()?;

        // There are no more top level values.
        assert_eq!(reader.next()?, None);

        // Asking for more still results in `None`
        assert_eq!(reader.next()?, None);

        Ok(())
    }

    #[test]
    fn test_read_multiple_top_level_values_with_comments() -> IonResult<()> {
        let ion_data = r#"
            /*
                Arrokoth is a trans-Neptunian object in the Kuiper belt.
                It is a contact binary composed of two plenetesimals joined
                along their major axes.
            */
            "(486958) 2014 MU69" // Original designation
            2014-06-26T // Date of discovery
            km::36 // width
        "#;

        let reader = &mut TextReader::new(ion_data);

        next_type(reader, IonType::String, false);
        assert_eq!(reader.read_string()?.unwrap(), String::from("(486958) 2014 MU69"));

        next_type(reader, IonType::Timestamp, false);
        assert_eq!(reader.read_timestamp()?.unwrap(),
                   Timestamp::with_ymd(2014, 6, 26).build().unwrap());
        // TODO: Check for 'km' annotation after change to OwnedSymbolToken
        next_type(reader, IonType::Integer, false);
        assert_eq!(reader.read_i64()?.unwrap(), 36);
        Ok(())
    }

    #[test]
    fn test_text_read_multiple_annotated_top_level_values() -> IonResult<()> {
        let ion_data = r#"
            mercury::null
            venus::'earth'::true
            $17::mars::5
            jupiter::5e0
            'saturn'::5.5
            $100::$200::$300::2021-09-25T
            'uranus'::foo
            neptune::"hello"
            $55::{foo: bar}
            pluto::[1, 2, 3]
            haumea::makemake::eris::ceres::(++ -- &&&&&)
        "#;
        // TODO: Check for annotations after OwnedSymbolToken

        let reader = &mut TextReader::new(ion_data);
        next_type(reader, IonType::Null, true);

        next_type(reader, IonType::Boolean, false);
        assert_eq!(reader.read_bool()?.unwrap(), true);

        next_type(reader, IonType::Integer, false);
        assert_eq!(reader.read_i64()?.unwrap(), 5);

        next_type(reader, IonType::Float, false);
        assert_eq!(reader.read_f64()?.unwrap(), 5.0f64);

        next_type(reader, IonType::Decimal, false);
        assert_eq!(reader.read_decimal()?.unwrap(), Decimal::new(55, -1));

        next_type(reader, IonType::Timestamp, false);
        assert_eq!(reader.read_timestamp()?.unwrap(), Timestamp::with_ymd(2021, 9, 25).build().unwrap());

        next_type(reader, IonType::Symbol, false);
        // TODO: Read in symbol 'foo' after updating read_symbol to use OwnedSymbolToken
        // assert_eq!(reader.read_symbol_id()?.unwrap(), text_token("foo"));

        next_type(reader, IonType::String, false);
        assert_eq!(reader.read_string()?.unwrap(), "hello".to_string());

        // ===== CONTAINERS =====

        // Reading a struct: $55::{foo: bar}
        next_type(reader, IonType::Struct, false);
        reader.step_in()?;
        next_type(reader, IonType::Symbol, false);
        // TODO: Read symbol after ... OwnedSymbolToken
        // TODO: Read field ID after ... OwnedSymbolToken
        assert_eq!(reader.next()?, None);
        reader.step_out()?;

        // Reading a list: pluto::[1, 2, 3]
        next_type(reader, IonType::List, false);
        reader.step_in()?;
        next_type(reader, IonType::Integer, false);
        assert_eq!(reader.read_i64()?.unwrap(), 1);
        next_type(reader, IonType::Integer, false);
        assert_eq!(reader.read_i64()?.unwrap(), 2);
        next_type(reader, IonType::Integer, false);
        assert_eq!(reader.read_i64()?.unwrap(), 3);
        assert_eq!(reader.next()?, None);
        reader.step_out()?;

        // Reading an s-expression: haumea::makemake::eris::ceres::(++ -- &&&&&)
        next_type(reader, IonType::SExpression, false);
        reader.step_in()?;
        // TODO: Read the three symbols ... OST
        next_type(reader, IonType::Symbol, false);
        next_type(reader, IonType::Symbol, false);
        next_type(reader, IonType::Symbol, false);
        assert_eq!(reader.next()?, None);
        reader.step_out()?;

        // There are no more top level values.
        assert_eq!(reader.next()?, None);

        // Asking for more still results in `None`
        assert_eq!(reader.next()?, None);

        Ok(())
    }
}
