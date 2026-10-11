//! Conservative receive-service discontinuity detection and bounded serial I/O.
//! No clock here grants observation authority. A suspend-inclusive clock only
//! revokes input provenance when the runtime Instant clock did not advance.

use web_time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) struct ServiceTime {
    before: Instant,
    after: Instant,
    continuous: Duration,
}

impl ServiceTime {
    pub(crate) fn now() -> Self {
        let before = Instant::now();
        #[cfg(unix)]
        let continuous = {
            #[cfg(target_os = "linux")]
            let clock = rustix::time::ClockId::Boottime;
            #[cfg(not(target_os = "linux"))]
            let clock = rustix::time::ClockId::Monotonic;
            let time = rustix::time::clock_gettime(clock);
            Duration::new(time.tv_sec as u64, time.tv_nsec as u32)
        };
        #[cfg(not(unix))]
        let continuous = {
            // Windows Instant/QPC includes suspension. It is already the
            // runtime authority clock, unlike Linux CLOCK_MONOTONIC.
            static ORIGIN: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
            before.saturating_duration_since(*ORIGIN.get_or_init(|| before))
        };
        let after = Instant::now();
        Self {
            before,
            after,
            continuous,
        }
    }

    pub(crate) fn discontinuity_since(self, earlier: Self) -> bool {
        // Capture uncertainty is an interval, not an invented jitter threshold.
        // Elapsed continuous time beyond even the largest runnable-clock bound
        // means old input cannot be assigned a trustworthy current age.
        self.continuous < earlier.continuous
            || self.continuous.saturating_sub(earlier.continuous)
                > self.after.saturating_duration_since(earlier.before)
    }

    fn attempt_expired(self, earlier: Self) -> bool {
        self.before >= earlier.after + crate::controller::REPORT_FRESHNESS_TIMEOUT
            || self.continuous.saturating_sub(earlier.continuous)
                >= crate::controller::REPORT_FRESHNESS_TIMEOUT
            || self.discontinuity_since(earlier)
    }

    fn attempt_remaining(self, earlier: Self) -> Option<Duration> {
        if self.attempt_expired(earlier) {
            return None;
        }
        let elapsed = self
            .before
            .saturating_duration_since(earlier.after)
            .max(self.continuous.saturating_sub(earlier.continuous));
        Some(crate::controller::REPORT_FRESHNESS_TIMEOUT.saturating_sub(elapsed))
    }
}

pub(crate) fn write_frame(
    port: &mut dyn serialport::SerialPort,
    frame: crate::device::OutboundFrame,
) -> Result<(), String> {
    let started = ServiceTime::now();
    let old_timeout = port.timeout();
    let timeout = if old_timeout.is_zero() {
        crate::controller::REPORT_FRESHNESS_TIMEOUT
    } else {
        old_timeout.min(crate::controller::REPORT_FRESHNESS_TIMEOUT)
    };
    let bytes: [u8; crate::device::OUTBOUND_FRAME_SIZE] = frame.into();
    let mut written = 0;
    while written < bytes.len() {
        let Some(remaining) = ServiceTime::now().attempt_remaining(started) else {
            return Err(
                "serial command attempt timed out or crossed suspension; outcome is uncertain"
                    .to_owned(),
            );
        };
        // Bound every syscall by the remainder of the whole attempt, not a
        // fresh ten-second allowance for each partial write or interruption.
        port.set_timeout(timeout.min(remaining))
            .map_err(|e| format!("cannot bound serial write: {e}"))?;
        match port.write(&bytes[written..]) {
            Ok(0) => return Err("serial write made no progress".to_owned()),
            Ok(count) => written += count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("serial write failed: {e}")),
        }
    }
    if ServiceTime::now().attempt_expired(started) {
        return Err("late serial write completion retired; outcome is uncertain".to_owned());
    }
    port.set_timeout(old_timeout)
        .map_err(|e| format!("serial timeout restoration failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inclusive_attempt_deadline_and_suspend_detection_have_no_granting_path() {
        let t0 = Instant::now();
        let start = ServiceTime {
            before: t0,
            after: t0,
            continuous: Duration::from_secs(100),
        };
        let at = |delta: Duration| ServiceTime {
            before: t0 + delta,
            after: t0 + delta,
            continuous: start.continuous + delta,
        };
        assert!(
            at(Duration::from_secs(10).saturating_sub(Duration::from_nanos(1)))
                .attempt_remaining(start)
                .is_some_and(|remaining| remaining == Duration::from_nanos(1))
        );
        assert!(at(Duration::from_secs(10)).attempt_expired(start));
        let suspended = ServiceTime {
            before: t0 + Duration::from_secs(1),
            after: t0 + Duration::from_secs(1),
            continuous: start.continuous + Duration::from_secs(2),
        };
        assert!(suspended.discontinuity_since(start));
        assert!(suspended.attempt_expired(start));
        // Clock capture spans one ns: normal read uncertainty is not mistaken
        // for suspension, but even a later proven 1ns excess fails closed.
        let bounded = ServiceTime {
            after: t0 + Duration::from_secs(1) + Duration::from_nanos(1),
            continuous: start.continuous + Duration::from_secs(1) + Duration::from_nanos(1),
            ..suspended
        };
        assert!(!bounded.discontinuity_since(start));
    }
}

// Software-only transport seam: the real PTY write completes, then input may
// arrive before the executor observes its return. No authority policy is mocked.
#[cfg(all(test, unix))]
pub(crate) mod fixture {
    use serialport::{ClearBuffer, DataBits, FlowControl, Parity, SerialPort, StopBits};
    use std::io::{Read, Write};
    use std::time::Duration;

    pub(crate) struct WriteHook {
        pub(crate) port: Box<dyn SerialPort>,
        pub(crate) hook: Box<dyn FnMut() + Send>,
    }

    impl Read for WriteHook {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.port.read(bytes)
        }
    }

    impl Write for WriteHook {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let count = self.port.write(bytes)?;
            (self.hook)();
            Ok(count)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.port.flush()
        }
    }

    macro_rules! forward {
        () => {};
        (fn $name:ident(&self $(, $arg:ident: $type:ty)*) -> $result:ty; $($rest:tt)*) => {
            fn $name(&self $(, $arg: $type)*) -> $result {
                self.port.$name($($arg),*)
            }
            forward! { $($rest)* }
        };
        (fn $name:ident(&mut self $(, $arg:ident: $type:ty)*) -> $result:ty; $($rest:tt)*) => {
            fn $name(&mut self $(, $arg: $type)*) -> $result {
                self.port.$name($($arg),*)
            }
            forward! { $($rest)* }
        };
    }

    impl SerialPort for WriteHook {
        forward! {
            fn name(&self) -> Option<String>;
            fn baud_rate(&self) -> serialport::Result<u32>;
            fn data_bits(&self) -> serialport::Result<DataBits>;
            fn flow_control(&self) -> serialport::Result<FlowControl>;
            fn parity(&self) -> serialport::Result<Parity>;
            fn stop_bits(&self) -> serialport::Result<StopBits>;
            fn timeout(&self) -> Duration;
            fn set_baud_rate(&mut self, value: u32) -> serialport::Result<()>;
            fn set_data_bits(&mut self, value: DataBits) -> serialport::Result<()>;
            fn set_flow_control(&mut self, value: FlowControl) -> serialport::Result<()>;
            fn set_parity(&mut self, value: Parity) -> serialport::Result<()>;
            fn set_stop_bits(&mut self, value: StopBits) -> serialport::Result<()>;
            fn set_timeout(&mut self, value: Duration) -> serialport::Result<()>;
            fn write_request_to_send(&mut self, value: bool) -> serialport::Result<()>;
            fn write_data_terminal_ready(&mut self, value: bool) -> serialport::Result<()>;
            fn read_clear_to_send(&mut self) -> serialport::Result<bool>;
            fn read_data_set_ready(&mut self) -> serialport::Result<bool>;
            fn read_ring_indicator(&mut self) -> serialport::Result<bool>;
            fn read_carrier_detect(&mut self) -> serialport::Result<bool>;
            fn bytes_to_read(&self) -> serialport::Result<u32>;
            fn bytes_to_write(&self) -> serialport::Result<u32>;
            fn clear(&self, value: ClearBuffer) -> serialport::Result<()>;
            fn try_clone(&self) -> serialport::Result<Box<dyn SerialPort>>;
            fn set_break(&self) -> serialport::Result<()>;
            fn clear_break(&self) -> serialport::Result<()>;
        }
    }
}
