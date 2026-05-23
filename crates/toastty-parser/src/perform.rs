use vte::Params;

/// Handler trait. Implementors receive every parsed event from a
/// [`crate::Parser`]. All methods default to no-ops; override the ones
/// you care about.
///
/// APC is split into three callbacks ([`Self::apc_start`],
/// [`Self::apc_chunk`], [`Self::apc_end`]) so handlers can stream
/// large payloads without forcing the parser to buffer them.
/// Handlers that prefer the whole-payload form can wrap themselves
/// in [`BufferingApcHandler`].
pub trait Perform {
    fn print(&mut self, _c: char) {}
    fn execute(&mut self, _byte: u8) {}
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    fn csi_dispatch(
        &mut self,
        _params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
    }
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}

    /// Beginning of an APC payload (`ESC _`). The header parameters
    /// (e.g. kitty graphics `a=T,f=24,s=...`) appear in subsequent
    /// [`Self::apc_chunk`] calls — the parser does not split header
    /// from body, since the framing has no fixed separator.
    fn apc_start(&mut self) {}

    /// A chunk of APC payload bytes. Called zero or more times
    /// between [`Self::apc_start`] and [`Self::apc_end`]. Chunk
    /// boundaries are not semantically meaningful — concatenate to
    /// reassemble.
    fn apc_chunk(&mut self, _bytes: &[u8]) {}

    /// End of the APC payload (ST seen).
    fn apc_end(&mut self) {}
}

/// Adapter that buffers APC chunks into a single `Vec<u8>` for
/// handlers that prefer whole-payload semantics. Wraps an inner
/// handler and forwards every other event verbatim.
///
/// Note this defeats the point of streaming for large payloads;
/// only use for handlers that genuinely need the whole APC at once.
#[derive(Debug, Default)]
pub struct BufferingApcHandler<H: BufferedApc> {
    inner: H,
    buf: Vec<u8>,
}

impl<H: BufferedApc> BufferingApcHandler<H> {
    pub fn new(inner: H) -> Self {
        Self {
            inner,
            buf: Vec::new(),
        }
    }

    pub fn into_inner(self) -> H {
        self.inner
    }

    pub fn inner(&self) -> &H {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut H {
        &mut self.inner
    }
}

/// Companion trait for [`BufferingApcHandler`]: implement `apc` with
/// the full payload, and the rest of [`Perform`]'s methods as needed.
pub trait BufferedApc: Perform {
    fn apc(&mut self, payload: &[u8]);
}

impl<H: BufferedApc> Perform for BufferingApcHandler<H> {
    fn print(&mut self, c: char) {
        self.inner.print(c);
    }
    fn execute(&mut self, byte: u8) {
        self.inner.execute(byte);
    }
    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.inner.hook(params, intermediates, ignore, action);
    }
    fn put(&mut self, byte: u8) {
        self.inner.put(byte);
    }
    fn unhook(&mut self) {
        self.inner.unhook();
    }
    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.inner.osc_dispatch(params, bell_terminated);
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.inner
            .csi_dispatch(params, intermediates, ignore, action);
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.inner.esc_dispatch(intermediates, ignore, byte);
    }

    fn apc_start(&mut self) {
        self.buf.clear();
    }
    fn apc_chunk(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }
    fn apc_end(&mut self) {
        self.inner.apc(&self.buf);
        self.buf.clear();
    }
}

/// Internal adapter: bridges our [`Perform`] trait onto vte's
/// `Perform` trait so we can drive `vte::Parser` while keeping our
/// own trait surface.
pub(crate) struct VteAdapter<'a, P: Perform + ?Sized> {
    pub inner: &'a mut P,
}

impl<P: Perform + ?Sized> vte::Perform for VteAdapter<'_, P> {
    fn print(&mut self, c: char) {
        self.inner.print(c);
    }
    fn execute(&mut self, byte: u8) {
        self.inner.execute(byte);
    }
    fn hook(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.inner.hook(params, intermediates, ignore, action);
    }
    fn put(&mut self, byte: u8) {
        self.inner.put(byte);
    }
    fn unhook(&mut self) {
        self.inner.unhook();
    }
    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        self.inner.osc_dispatch(params, bell_terminated);
    }
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        self.inner
            .csi_dispatch(params, intermediates, ignore, action);
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        self.inner.esc_dispatch(intermediates, ignore, byte);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exercise every default method — coverage gate would miss them otherwise.
    #[test]
    fn perform_defaults_are_callable() {
        struct Nop;
        impl Perform for Nop {}
        let mut nop = Nop;
        nop.print('x');
        nop.execute(0x07);
        nop.hook(&Params::default(), b"", false, 'q');
        nop.put(b'a');
        nop.unhook();
        nop.osc_dispatch(&[b"0", b"title"], false);
        nop.csi_dispatch(&Params::default(), b"", false, 'm');
        nop.esc_dispatch(b"", false, b'=');
        nop.apc_start();
        nop.apc_chunk(b"data");
        nop.apc_end();
    }

    #[test]
    fn buffering_apc_handler_assembles_chunks() {
        #[derive(Default)]
        struct Catcher {
            apc_payloads: Vec<Vec<u8>>,
        }
        impl Perform for Catcher {}
        impl BufferedApc for Catcher {
            fn apc(&mut self, payload: &[u8]) {
                self.apc_payloads.push(payload.to_vec());
            }
        }
        let mut h = BufferingApcHandler::new(Catcher::default());
        h.apc_start();
        h.apc_chunk(b"hel");
        h.apc_chunk(b"lo");
        h.apc_end();
        h.apc_start();
        h.apc_chunk(b"world");
        h.apc_end();
        assert_eq!(
            h.inner().apc_payloads,
            vec![b"hello".to_vec(), b"world".to_vec()]
        );
    }

    #[test]
    fn buffering_apc_handler_forwards_other_events() {
        #[derive(Default)]
        struct Counter {
            prints: u32,
            execs: u32,
            csis: u32,
            oscs: u32,
            escs: u32,
            hooks: u32,
            puts: u32,
            unhooks: u32,
        }
        impl Perform for Counter {
            fn print(&mut self, _c: char) {
                self.prints += 1;
            }
            fn execute(&mut self, _b: u8) {
                self.execs += 1;
            }
            fn csi_dispatch(&mut self, _: &Params, _: &[u8], _: bool, _: char) {
                self.csis += 1;
            }
            fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {
                self.oscs += 1;
            }
            fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {
                self.escs += 1;
            }
            fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {
                self.hooks += 1;
            }
            fn put(&mut self, _: u8) {
                self.puts += 1;
            }
            fn unhook(&mut self) {
                self.unhooks += 1;
            }
        }
        impl BufferedApc for Counter {
            fn apc(&mut self, _: &[u8]) {}
        }
        let mut h = BufferingApcHandler::new(Counter::default());
        h.print('a');
        h.execute(0x07);
        h.csi_dispatch(&Params::default(), b"", false, 'm');
        h.osc_dispatch(&[b"0"], false);
        h.esc_dispatch(b"", false, b'c');
        h.hook(&Params::default(), b"", false, 'q');
        h.put(b'x');
        h.unhook();
        let c = h.into_inner();
        assert_eq!(
            (
                c.prints, c.execs, c.csis, c.oscs, c.escs, c.hooks, c.puts, c.unhooks
            ),
            (1, 1, 1, 1, 1, 1, 1, 1)
        );
    }
}
