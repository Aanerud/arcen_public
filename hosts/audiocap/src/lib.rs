use std::sync::OnceLock;

use arcen_telemetry::CorrelationId;

static SESSION_LOG_ID: OnceLock<Option<CorrelationId>> = OnceLock::new();

fn initialize_session_log_id() -> Result<(), &'static str> {
    let value = match std::env::var("ARCEN_SESSION_LOG_ID") {
        Ok(value) => Some(
            CorrelationId::parse_uuid(value)
                .map_err(|_| "ARCEN_SESSION_LOG_ID must be a canonical lowercase UUID")?,
        ),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err("ARCEN_SESSION_LOG_ID must be valid UTF-8");
        }
    };
    SESSION_LOG_ID
        .set(value)
        .map_err(|_| "session log id was already initialized")
}

fn diagnostic_line(message: &str) -> String {
    SESSION_LOG_ID.get().and_then(Option::as_ref).map_or_else(
        || format!("[audiocap] {message}"),
        |id| format!("[audiocap] sid={id} {message}"),
    )
}

#[cfg(target_os = "linux")]
mod linux {
    use std::cell::RefCell;
    use std::io::{self, Write};
    use std::rc::Rc;

    use psimple::Simple;
    use pulse::callbacks::ListResult;
    use pulse::context::{Context, FlagSet, State};
    use pulse::def::BufferAttr;
    use pulse::mainloop::standard::{IterateResult, Mainloop};
    use pulse::sample::{Format, Spec};
    use pulse::stream::Direction;

    const APP_NAME: &str = "arcen-audiocap";
    const DEFAULT_SAMPLE_RATE: u32 = 48_000;
    const DEFAULT_CHANNELS: u8 = 2;
    const DEFAULT_CHUNK_MS: u32 = 20;

    #[derive(Debug, Clone)]
    struct Args {
        source: Option<String>,
        sample_rate: u32,
        channels: u8,
        chunk_ms: u32,
        check_only: bool,
        print_source: bool,
    }

    pub fn run_with_args(args: &[String]) -> Result<(), String> {
        let args = parse_args(args.iter().skip(1).cloned())?;
        let source = match args.source.clone() {
            Some(source) => source,
            None => resolve_monitor_source()?,
        };

        if args.print_source {
            println!("{source}");
            return Ok(());
        }

        let spec = Spec {
            format: Format::S16le,
            rate: args.sample_rate,
            channels: args.channels,
        };
        if !spec.is_valid() {
            return Err(format!(
                "invalid PulseAudio sample spec: {} Hz, {} channels, S16le",
                args.sample_rate, args.channels
            ));
        }

        let chunk_bytes = chunk_bytes(args.sample_rate, args.channels, args.chunk_ms)?;
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: chunk_bytes as u32,
        };

        let recorder = Simple::new(
            None,
            APP_NAME,
            Direction::Record,
            Some(&source),
            "arcen-monitor-record",
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|error| format!("PulseAudio record stream failed for {source}: {error:?}"))?;

        eprintln!(
            "{}",
            crate::diagnostic_line(&format!(
                "source={source} format=s16le rate={} channels={} chunk_ms={}",
                args.sample_rate, args.channels, args.chunk_ms
            ))
        );

        if args.check_only {
            return Ok(());
        }

        capture_loop(recorder, chunk_bytes)
    }

    fn parse_args<I>(mut args: I) -> Result<Args, String>
    where
        I: Iterator<Item = String>,
    {
        let mut parsed = Args {
            source: None,
            sample_rate: DEFAULT_SAMPLE_RATE,
            channels: DEFAULT_CHANNELS,
            chunk_ms: DEFAULT_CHUNK_MS,
            check_only: false,
            print_source: false,
        };

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--source" => {
                    parsed.source = Some(next_value(&mut args, "--source")?);
                }
                "--sample-rate" => {
                    parsed.sample_rate = parse_value(&mut args, "--sample-rate")?;
                }
                "--channels" => {
                    parsed.channels = parse_value(&mut args, "--channels")?;
                }
                "--chunk-ms" => {
                    parsed.chunk_ms = parse_value(&mut args, "--chunk-ms")?;
                }
                "--check" => parsed.check_only = true,
                "--print-source" => parsed.print_source = true,
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => return Err(format!("unknown argument: {other}")),
            }
        }

        if parsed.channels == 0 {
            return Err("--channels must be greater than zero".to_string());
        }
        if parsed.sample_rate == 0 {
            return Err("--sample-rate must be greater than zero".to_string());
        }
        if parsed.chunk_ms == 0 {
            return Err("--chunk-ms must be greater than zero".to_string());
        }

        Ok(parsed)
    }

    fn next_value<I>(args: &mut I, flag: &str) -> Result<String, String>
    where
        I: Iterator<Item = String>,
    {
        args.next()
            .ok_or_else(|| format!("{flag} requires a value"))
    }

    fn parse_value<I, T>(args: &mut I, flag: &str) -> Result<T, String>
    where
        I: Iterator<Item = String>,
        T: std::str::FromStr,
        T::Err: std::fmt::Display,
    {
        next_value(args, flag)?
            .parse::<T>()
            .map_err(|error| format!("invalid value for {flag}: {error}"))
    }

    fn print_usage() {
        eprintln!(
            "Usage: arcen-audiocap [--source NAME] [--sample-rate HZ] [--channels N] [--chunk-ms MS] [--check] [--print-source]"
        );
    }

    fn chunk_bytes(sample_rate: u32, channels: u8, chunk_ms: u32) -> Result<usize, String> {
        let bytes = u64::from(sample_rate)
            .checked_mul(u64::from(channels))
            .and_then(|value| value.checked_mul(2))
            .and_then(|value| value.checked_mul(u64::from(chunk_ms)))
            .map(|value| value / 1000)
            .ok_or_else(|| "audio chunk size overflow".to_string())?;
        usize::try_from(bytes).map_err(|_| "audio chunk size too large".to_string())
    }

    fn capture_loop(recorder: Simple, chunk_bytes: usize) -> Result<(), String> {
        let mut stdout = io::stdout().lock();
        let mut buffer = vec![0_u8; chunk_bytes];
        loop {
            recorder
                .read(&mut buffer)
                .map_err(|error| format!("PulseAudio read failed: {error:?}"))?;
            stdout
                .write_all(&buffer)
                .map_err(|error| format!("stdout write failed: {error}"))?;
            stdout
                .flush()
                .map_err(|error| format!("stdout flush failed: {error}"))?;
        }
    }

    fn resolve_monitor_source() -> Result<String, String> {
        let mut connection = PulseConnection::connect()?;

        if let Some(default_sink) = connection.default_sink_name()? {
            if let Some(monitor) = connection.monitor_for_sink(&default_sink)? {
                return Ok(monitor);
            }
        }

        connection
            .first_monitor_source()?
            .ok_or_else(|| "no PulseAudio monitor source found".to_string())
    }

    struct PulseConnection {
        context: Context,
        mainloop: Mainloop,
    }

    impl PulseConnection {
        fn connect() -> Result<Self, String> {
            let mut mainloop = Mainloop::new()
                .ok_or_else(|| "failed to create PulseAudio mainloop".to_string())?;
            let mut context = Context::new(&mainloop, APP_NAME)
                .ok_or_else(|| "failed to create PulseAudio context".to_string())?;
            context
                .connect(None, FlagSet::NOFLAGS, None)
                .map_err(|error| format!("PulseAudio connect failed: {error:?}"))?;

            loop {
                iterate(&mut mainloop)?;
                match context.get_state() {
                    State::Ready => break,
                    State::Failed | State::Terminated => {
                        return Err(format!(
                            "PulseAudio context did not become ready: {:?}",
                            context.get_state()
                        ));
                    }
                    _ => {}
                }
            }

            Ok(Self { context, mainloop })
        }

        fn default_sink_name(&mut self) -> Result<Option<String>, String> {
            let value = Rc::new(RefCell::new(None::<String>));
            let done = Rc::new(RefCell::new(false));
            let value_cb = Rc::clone(&value);
            let done_cb = Rc::clone(&done);
            let _operation = self.context.introspect().get_server_info(move |info| {
                *value_cb.borrow_mut() = info.default_sink_name.as_ref().map(ToString::to_string);
                *done_cb.borrow_mut() = true;
            });
            self.wait_for(done)?;
            let out = value.borrow().clone();
            Ok(out)
        }

        fn monitor_for_sink(&mut self, sink_name: &str) -> Result<Option<String>, String> {
            let value = Rc::new(RefCell::new(None::<String>));
            let done = Rc::new(RefCell::new(false));
            let value_cb = Rc::clone(&value);
            let done_cb = Rc::clone(&done);
            let _operation =
                self.context
                    .introspect()
                    .get_sink_info_by_name(sink_name, move |result| match result {
                        ListResult::Item(info) => {
                            *value_cb.borrow_mut() =
                                info.monitor_source_name.as_ref().map(ToString::to_string);
                        }
                        ListResult::End | ListResult::Error => {
                            *done_cb.borrow_mut() = true;
                        }
                    });
            self.wait_for(done)?;
            let out = value.borrow().clone();
            Ok(out)
        }

        fn first_monitor_source(&mut self) -> Result<Option<String>, String> {
            let value = Rc::new(RefCell::new(None::<String>));
            let done = Rc::new(RefCell::new(false));
            let value_cb = Rc::clone(&value);
            let done_cb = Rc::clone(&done);
            let _operation = self
                .context
                .introspect()
                .get_source_info_list(move |result| match result {
                    ListResult::Item(info) => {
                        let mut out = value_cb.borrow_mut();
                        if out.is_none() && info.monitor_of_sink.is_some() {
                            *out = info.name.as_ref().map(ToString::to_string);
                        }
                    }
                    ListResult::End | ListResult::Error => {
                        *done_cb.borrow_mut() = true;
                    }
                });
            self.wait_for(done)?;
            let out = value.borrow().clone();
            Ok(out)
        }

        fn wait_for(&mut self, done: Rc<RefCell<bool>>) -> Result<(), String> {
            while !*done.borrow() {
                iterate(&mut self.mainloop)?;
                match self.context.get_state() {
                    State::Failed | State::Terminated => {
                        return Err("PulseAudio context ended during introspection".to_string());
                    }
                    _ => {}
                }
            }
            Ok(())
        }
    }

    fn iterate(mainloop: &mut Mainloop) -> Result<(), String> {
        match mainloop.iterate(true) {
            IterateResult::Success(_) => Ok(()),
            IterateResult::Quit(retval) => Err(format!("PulseAudio mainloop quit: {retval:?}")),
            IterateResult::Err(error) => Err(format!("PulseAudio mainloop error: {error:?}")),
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod non_linux {
    pub fn run_with_args(_args: &[String]) -> Result<(), String> {
        Err("arcen-audiocap currently supports Linux PulseAudio/PipeWire only".to_string())
    }
}

pub fn run_with_args(args: &[String]) {
    if let Err(error) = initialize_session_log_id() {
        eprintln!("[audiocap] ERROR: {error}");
        std::process::exit(2);
    }

    #[cfg(target_os = "linux")]
    let result = linux::run_with_args(args);
    #[cfg(not(target_os = "linux"))]
    let result = non_linux::run_with_args(args);

    if let Err(error) = result {
        eprintln!("{}", diagnostic_line(&error));
        std::process::exit(1);
    }
}

/// Entry point when this helper runs as its own binary.
pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    run_with_args(&args);
}
