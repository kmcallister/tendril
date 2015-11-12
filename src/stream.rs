// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Streams of tendrils.

use tendril::{Tendril, Atomicity};
use fmt;

use std::borrow::Cow;
use std::marker::PhantomData;

use encoding::{EncodingRef, RawDecoder};
use utf8;

/// Trait for types that can process a tendril.
///
/// This is a "push" interface, unlike the "pull" interface of
/// `Iterator<Item=Tendril<F>>`. The push interface matches
/// [html5ever][] and other incremental parsers with a similar
/// architecture.
///
/// [html5ever]: https://github.com/servo/html5ever
pub trait TendrilSink<F, A>
    where F: fmt::Format,
          A: Atomicity,
{
    /// Process this tendril.
    fn process(&mut self, t: Tendril<F, A>);

    /// Indicates the end of the stream.
    ///
    /// By default, does nothing.
    fn finish(&mut self) { }

    /// Indicates that an error has occurred.
    fn error(&mut self, desc: Cow<'static, str>);
}

/// Lossily decode UTF-8 in a byte stream and emit a Unicode (`StrTendril`) stream.
///
/// This does not allocate memory: the output is either subtendrils on the input,
/// on inline tendrils for a single code point.
pub struct Utf8LossyDecoder<Sink, A>
    where Sink: TendrilSink<fmt::UTF8, A>,
          A: Atomicity
{
    decoder: utf8::Decoder,
    sink: Sink,
    marker: PhantomData<A>,
}

impl<Sink, A> Utf8LossyDecoder<Sink, A>
    where Sink: TendrilSink<fmt::UTF8, A>,
          A: Atomicity,
{
    /// Create a new incremental validator.
    #[inline]
    pub fn new(sink: Sink) -> Self {
        Utf8LossyDecoder {
            decoder: utf8::Decoder::new(),
            sink: sink,
            marker: PhantomData,
        }
    }

    /// Consume the validator and obtain the sink.
    #[inline]
    pub fn into_sink(self) -> Sink {
        self.sink
    }
}

impl<Sink, A> TendrilSink<fmt::Bytes, A> for Utf8LossyDecoder<Sink, A>
    where Sink: TendrilSink<fmt::UTF8, A>,
          A: Atomicity,
{
    #[inline]
    fn process(&mut self, t: Tendril<fmt::Bytes, A>) {
        let mut input = &*t;
        loop {
            let (ch, s, result) = self.decoder.decode(input);
            if !ch.is_empty() {
                self.sink.process(Tendril::from_slice(&*ch));
            }
            if !s.is_empty() {
                // rust-utf8 promises that `s` is a subslice of `&*t`
                // so this substraction won’t underflow and `subtendril()` won’t panic.
                let offset = s.as_ptr() as usize - t.as_ptr() as usize;
                let subtendril = t.subtendril(offset as u32, s.len() as u32);
                unsafe {
                    self.sink.process(subtendril.reinterpret_without_validating());
                }
            }
            match result {
                utf8::Result::Ok | utf8::Result::Incomplete => break,
                utf8::Result::Error { remaining_input_after_error: remaining } => {
                    self.sink.error("invalid byte sequence".into());
                    self.sink.process(Tendril::from_slice(utf8::REPLACEMENT_CHARACTER));
                    input = remaining;
                }
            }
        }
    }

    #[inline]
    fn finish(&mut self) {
        if self.decoder.has_incomplete_sequence() {
            self.sink.error("incomplete byte sequence at end of stream".into());
            self.sink.process(Tendril::from_slice(utf8::REPLACEMENT_CHARACTER));
        }
        self.sink.finish();
    }

    #[inline]
    fn error(&mut self, desc: Cow<'static, str>) {
        self.sink.error(desc);
    }
}

/// Incrementally decode a byte stream to UTF-8.
///
/// This will write the decoded characters into new tendrils.
/// To validate UTF-8 without copying, see `Utf8LossyDecoder`
/// in this module.
pub struct Decoder<Sink, A>
    where Sink: TendrilSink<fmt::UTF8, A>,
          A: Atomicity {
    inner: DecoderInner<Sink, A>,
}

enum DecoderInner<Sink, A>
    where Sink: TendrilSink<fmt::UTF8, A>,
          A: Atomicity {
    Utf8(Utf8LossyDecoder<Sink, A>),
    Other(Box<RawDecoder>, Sink)
}

impl<Sink, A> Decoder<Sink, A>
    where Sink: TendrilSink<fmt::UTF8, A>,
          A: Atomicity,
{
    /// Create a new incremental decoder.
    #[inline]
    pub fn new(encoding: EncodingRef, sink: Sink) -> Decoder<Sink, A> {
        Decoder {
            inner: if encoding.name() == "utf-8" {
                DecoderInner::Utf8(Utf8LossyDecoder::new(sink))
            } else {
                DecoderInner::Other(encoding.raw_decoder(), sink)
            }
        }
    }

    /// Consume the decoder and obtain the sink.
    #[inline]
    pub fn into_sink(self) -> Sink {
        match self.inner {
            DecoderInner::Utf8(utf8) => utf8.into_sink(),
            DecoderInner::Other(_, sink) => sink,
        }
    }
}

impl<Sink, A> TendrilSink<fmt::Bytes, A> for Decoder<Sink, A>
    where Sink: TendrilSink<fmt::UTF8, A>,
          A: Atomicity,
{
    #[inline]
    fn process(&mut self, mut t: Tendril<fmt::Bytes, A>) {
        let (decoder, sink) = match self.inner {
            DecoderInner::Utf8(ref mut utf8) => return utf8.process(t),
            DecoderInner::Other(ref mut decoder, ref mut sink) => (decoder, sink),
        };

        let mut out = Tendril::new();
        loop {
            match decoder.raw_feed(&*t, &mut out) {
                (_, Some(err)) => {
                    out.push_char('\u{fffd}');
                    sink.error(err.cause);
                    debug_assert!(err.upto >= 0);
                    t.pop_front(err.upto as u32);
                    // continue loop and process remainder of t
                }
                (_, None) => break,
            }
        }
        if out.len() > 0 {
            sink.process(out);
        }
    }

    #[inline]
    fn finish(&mut self) {
        let (decoder, sink) = match self.inner {
            DecoderInner::Utf8(ref mut utf8) => return utf8.finish(),
            DecoderInner::Other(ref mut decoder, ref mut sink) => (decoder, sink),
        };

        let mut out = Tendril::new();
        if let Some(err) = decoder.raw_finish(&mut out) {
            out.push_char('\u{fffd}');
            sink.error(err.cause);
        }
        if out.len() > 0 {
            sink.process(out);
        }
        sink.finish();
    }

    #[inline]
    fn error(&mut self, desc: Cow<'static, str>) {
        match self.inner {
            DecoderInner::Utf8(ref mut utf8) => utf8.error(desc),
            DecoderInner::Other(_, ref mut sink) => sink.error(desc),
        }
    }
}

#[cfg(test)]
mod test {
    use super::{TendrilSink, Decoder, Utf8LossyDecoder};
    use tendril::{Tendril, Atomicity, SliceExt, NonAtomic};
    use fmt;
    use std::borrow::Cow;
    use encoding::EncodingRef;
    use encoding::all as enc;

    struct Accumulate<A>
        where A: Atomicity,
    {
        tendrils: Vec<Tendril<fmt::UTF8, A>>,
        errors: Vec<String>,
    }

    impl<A> Accumulate<A>
        where A: Atomicity,
    {
        fn new() -> Accumulate<A> {
            Accumulate {
                tendrils: vec![],
                errors: vec![],
            }
        }
    }

    impl<A> TendrilSink<fmt::UTF8, A> for Accumulate<A>
        where A: Atomicity,
    {
        fn process(&mut self, t: Tendril<fmt::UTF8, A>) {
            self.tendrils.push(t);
        }

        fn error(&mut self, desc: Cow<'static, str>) {
            self.errors.push(desc.into_owned());
        }
    }

    fn check_validate(input: &[&[u8]], expected: &[&str], errs: usize) {
        let mut validator = Utf8LossyDecoder::new(Accumulate::<NonAtomic>::new());
        for x in input {
            validator.process(x.to_tendril());
        }
        validator.finish();

        let Accumulate { tendrils, errors } = validator.into_sink();
        assert_eq!(expected, &*tendrils.iter().map(|t| &**t).collect::<Vec<_>>());
        assert_eq!(errs, errors.len());
    }

    #[test]
    fn validate_utf8() {
        check_validate(&[], &[], 0);
        check_validate(&[b""], &[], 0);
        check_validate(&[b"xyz"], &["xyz"], 0);
        check_validate(&[b"x", b"y", b"z"], &["x", "y", "z"], 0);

        check_validate(&[b"xy\xEA\x99\xAEzw"], &["xy\u{a66e}zw"], 0);
        check_validate(&[b"xy\xEA", b"\x99\xAEzw"], &["xy", "\u{a66e}", "zw"], 0);
        check_validate(&[b"xy\xEA\x99", b"\xAEzw"], &["xy", "\u{a66e}", "zw"], 0);
        check_validate(&[b"xy\xEA", b"\x99", b"\xAEzw"], &["xy", "\u{a66e}", "zw"], 0);
        check_validate(&[b"\xEA", b"", b"\x99", b"", b"\xAE"], &["\u{a66e}"], 0);
        check_validate(&[b"", b"\xEA", b"", b"\x99", b"", b"\xAE", b""], &["\u{a66e}"], 0);

        check_validate(&[b"xy\xEA", b"\xFF", b"\x99\xAEz"],
            &["xy", "\u{fffd}", "\u{fffd}", "\u{fffd}", "\u{fffd}", "z"], 4);
        check_validate(&[b"xy\xEA\x99", b"\xFFz"],
            &["xy", "\u{fffd}", "\u{fffd}", "z"], 2);

        check_validate(&[b"\xC5\x91\xC5\x91\xC5\x91"], &["őőő"], 0);
        check_validate(&[b"\xC5\x91", b"\xC5\x91", b"\xC5\x91"], &["ő", "ő", "ő"], 0);
        check_validate(&[b"\xC5", b"\x91\xC5", b"\x91\xC5", b"\x91"],
            &["ő", "ő", "ő"], 0);
        check_validate(&[b"\xC5", b"\x91\xff", b"\x91\xC5", b"\x91"],
            &["ő", "\u{fffd}", "\u{fffd}", "ő"], 2);

        // incomplete char at end of input
        check_validate(&[b"\xC0"], &["\u{fffd}"], 1);
        check_validate(&[b"\xEA\x99"], &["\u{fffd}"], 1);
    }

    fn check_decode(enc: EncodingRef, input: &[&[u8]], expected: &str, errs: usize) {
        let mut decoder = Decoder::new(enc, Accumulate::new());
        for x in input {
            decoder.process(x.to_tendril());
        }
        decoder.finish();

        let Accumulate { tendrils, errors } = decoder.into_sink();
        let mut tendril: Tendril<fmt::UTF8> = Tendril::new();
        for t in tendrils {
            tendril.push_tendril(&t);
        }
        assert_eq!(expected, &*tendril);
        assert_eq!(errs, errors.len());
    }

    #[test]
    fn decode_ascii() {
        check_decode(enc::ASCII, &[], "", 0);
        check_decode(enc::ASCII, &[b""], "", 0);
        check_decode(enc::ASCII, &[b"xyz"], "xyz", 0);
        check_decode(enc::ASCII, &[b"xy", b"", b"", b"z"], "xyz", 0);
        check_decode(enc::ASCII, &[b"x", b"y", b"z"], "xyz", 0);

        check_decode(enc::ASCII, &[b"\xFF"], "\u{fffd}", 1);
        check_decode(enc::ASCII, &[b"x\xC0yz"], "x\u{fffd}yz", 1);
        check_decode(enc::ASCII, &[b"x", b"\xC0y", b"z"], "x\u{fffd}yz", 1);
        check_decode(enc::ASCII, &[b"x\xC0yz\xFF\xFFw"], "x\u{fffd}yz\u{fffd}\u{fffd}w", 3);
    }

    #[test]
    fn decode_utf8() {
        check_decode(enc::UTF_8, &[], "", 0);
        check_decode(enc::UTF_8, &[b""], "", 0);
        check_decode(enc::UTF_8, &[b"xyz"], "xyz", 0);
        check_decode(enc::UTF_8, &[b"x", b"y", b"z"], "xyz", 0);

        check_decode(enc::UTF_8, &[b"\xEA\x99\xAE"], "\u{a66e}", 0);
        check_decode(enc::UTF_8, &[b"\xEA", b"\x99\xAE"], "\u{a66e}", 0);
        check_decode(enc::UTF_8, &[b"\xEA\x99", b"\xAE"], "\u{a66e}", 0);
        check_decode(enc::UTF_8, &[b"\xEA", b"\x99", b"\xAE"], "\u{a66e}", 0);
        check_decode(enc::UTF_8, &[b"\xEA", b"", b"\x99", b"", b"\xAE"], "\u{a66e}", 0);
        check_decode(enc::UTF_8, &[b"", b"\xEA", b"", b"\x99", b"", b"\xAE", b""], "\u{a66e}", 0);

        check_decode(enc::UTF_8, &[b"xy\xEA", b"\x99\xAEz"], "xy\u{a66e}z", 0);
        check_decode(enc::UTF_8, &[b"xy\xEA", b"\xFF", b"\x99\xAEz"],
            "xy\u{fffd}\u{fffd}\u{fffd}\u{fffd}z", 4);
        check_decode(enc::UTF_8, &[b"xy\xEA\x99", b"\xFFz"],
            "xy\u{fffd}\u{fffd}z", 2);

        // incomplete char at end of input
        check_decode(enc::UTF_8, &[b"\xC0"], "\u{fffd}", 1);
        check_decode(enc::UTF_8, &[b"\xEA\x99"], "\u{fffd}", 1);
    }

    #[test]
    fn decode_koi8_u() {
        check_decode(enc::KOI8_U, &[b"\xfc\xce\xc5\xd2\xc7\xc9\xd1"], "Энергия", 0);
        check_decode(enc::KOI8_U, &[b"\xfc\xce", b"\xc5\xd2\xc7\xc9\xd1"], "Энергия", 0);
        check_decode(enc::KOI8_U, &[b"\xfc\xce", b"\xc5\xd2\xc7", b"\xc9\xd1"], "Энергия", 0);
        check_decode(enc::KOI8_U, &[b"\xfc\xce", b"", b"\xc5\xd2\xc7", b"\xc9\xd1", b""], "Энергия", 0);
    }

    #[test]
    fn decode_windows_949() {
        check_decode(enc::WINDOWS_949, &[], "", 0);
        check_decode(enc::WINDOWS_949, &[b""], "", 0);
        check_decode(enc::WINDOWS_949, &[b"\xbe\xc8\xb3\xe7"], "안녕", 0);
        check_decode(enc::WINDOWS_949, &[b"\xbe", b"\xc8\xb3\xe7"], "안녕", 0);
        check_decode(enc::WINDOWS_949, &[b"\xbe", b"", b"\xc8\xb3\xe7"], "안녕", 0);
        check_decode(enc::WINDOWS_949, &[b"\xbe\xc8\xb3\xe7\xc7\xcf\xbc\xbc\xbf\xe4"],
            "안녕하세요", 0);
        check_decode(enc::WINDOWS_949, &[b"\xbe\xc8\xb3\xe7\xc7"], "안녕\u{fffd}", 1);

        check_decode(enc::WINDOWS_949, &[b"\xbe", b"", b"\xc8\xb3"], "안\u{fffd}", 1);
        check_decode(enc::WINDOWS_949, &[b"\xbe\x28\xb3\xe7"], "\u{fffd}(녕", 1);
    }
}
