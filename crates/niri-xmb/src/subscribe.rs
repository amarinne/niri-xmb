use crate::ipc;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Protocol version spoken by the state stream.
pub const STREAM_PROTOCOL: u32 = 1;
/// Snapshot schema version carried inside snapshot frames.
pub const SNAPSHOT_SCHEMA: u32 = 1;

const MAX_FRAME_BYTES: usize = 256 * 1024;
const MAX_SUBSCRIBERS: usize = 8;
const WRITE_TIMEOUT: Duration = Duration::from_millis(500);
/// Backoff sequence start for the generic client.
const RECONNECT_MIN: Duration = Duration::from_millis(100);
const RECONNECT_MAX: Duration = Duration::from_secs(2);

/// One shared newest frame plus the live subscriber count.
#[derive(Default)]
struct HubState {
    latest: Option<Arc<Published>>,
    /// Increments on every publish.
    ///
    /// Freshness is per-publish, not per generation: a title-only
    /// change moves `content_revision` and deliberately leaves
    /// `generation` alone, and subscribers must still receive it.
    sequence: u64,
    active: usize,
}

enum Published {
    /// A complete NDJSON line, newline included.
    Snapshot { line: Arc<String> },
    /// The serialized snapshot exceeded the frame bound.
    Oversize,
}

/// Fanout point for versioned snapshot frames.
///
/// The daemon serializes one frame per generation; every writer sends that
/// shared frame. A slow subscriber skips older generations because each frame
/// is complete on its own. Subscribers free their slots when a write fails,
/// which is the only signal a half-closed local socket reliably provides.
pub struct SubscriptionHub {
    instance: String,
    state: Mutex<HubState>,
    signal: Condvar,
}

impl SubscriptionHub {
    pub fn new() -> Self {
        Self {
            instance: generate_instance(),
            state: Mutex::new(HubState::default()),
            signal: Condvar::new(),
        }
    }

    /// Store the newest frame and wake every writer.
    ///
    /// Serialization happens once per generation regardless of how many
    /// subscribers exist.
    pub fn publish(&self, generation: u64, snapshot_body: &str) {
        // Built by hand because snapshot_body is already serialised JSON;
        // json! would encode it a second time as a string.
        let raw = format!(
            "{{\"type\":\"snapshot\",\"protocol\":{STREAM_PROTOCOL},\"schema\":{SNAPSHOT_SCHEMA},\
             \"instance\":\"{}\",\"generation\":{generation},\"snapshot\":{snapshot_body}}}",
            self.instance
        );
        let published = if raw.len() > MAX_FRAME_BYTES {
            Published::Oversize
        } else {
            Published::Snapshot {
                line: Arc::new(raw + "\n"),
            }
        };
        let mut state = self.state.lock().unwrap();
        state.sequence += 1;
        state.latest = Some(Arc::new(published));
        drop(state);
        self.signal.notify_all();
    }

    /// Take ownership of an accepted control connection and stream frames.
    ///
    /// The connection must already have sent `subscribe <version>`. Every
    /// rejection path answers with one structured error frame before close.
    pub fn attach(self: &Arc<Self>, stream: UnixStream, version: u32) {
        if version != STREAM_PROTOCOL {
            write_error(
                &stream,
                "unsupported_protocol",
                &format!("unsupported stream protocol {version}"),
                false,
                &self.instance,
            );
            return;
        }
        if !peer_uid_matches(&stream) {
            write_error(
                &stream,
                "forbidden_peer",
                "subscriber uid differs from daemon uid",
                false,
                &self.instance,
            );
            return;
        }
        let mut writer = match stream.try_clone() {
            Ok(writer) => writer,
            Err(_) => return,
        };
        {
            let mut state = self.state.lock().unwrap();
            if state.active >= MAX_SUBSCRIBERS {
                drop(state);
                write_error(
                    &stream,
                    "subscriber_limit",
                    "no free subscriber slot",
                    true,
                    &self.instance,
                );
                return;
            }
            state.active += 1;
        }
        // One release per connection, whichever side notices the client left
        // first: the reader seeing EOF, or a failed write.
        let closed = Arc::new(AtomicBool::new(false));
        let hello = serde_json::json!({
            "type": "hello",
            "protocol": STREAM_PROTOCOL,
            "schema": SNAPSHOT_SCHEMA,
            "instance": self.instance,
        })
        .to_string()
            + "\n";
        let _ = writer.write_all(hello.as_bytes());
        // Watch for the client hanging up. run_writer blocks on the condvar
        // until a newer generation exists, so without this a disconnected
        // subscriber would hold its slot until the next publish -- on an idle
        // desktop, forever.
        if let Ok(reader) = stream.try_clone() {
            let hub = Arc::clone(self);
            let closed = Arc::clone(&closed);
            thread::Builder::new()
                .name("niri-xmb-subscribe-eof".into())
                .spawn(move || {
                    let mut reader = reader;
                    let mut discard = [0u8; 64];
                    // Subscribers are not expected to say anything; anything
                    // they do send is ignored. Only EOF or an error matters.
                    while let Ok(read) = reader.read(&mut discard) {
                        if read == 0 {
                            break;
                        }
                    }
                    if !closed.swap(true, Ordering::SeqCst) {
                        hub.release();
                    }
                    // Wake the writer so it sees `closed` and exits; it is
                    // parked waiting for a frame that may never be published.
                    hub.signal.notify_all();
                    let _ = reader.shutdown(Shutdown::Both);
                })
                .expect("failed to spawn subscription reader");
        }

        let hub = Arc::clone(self);
        thread::Builder::new()
            .name("niri-xmb-subscribe".into())
            .spawn(move || {
                hub.run_writer(writer, closed);
            })
            .expect("failed to spawn subscription writer");
    }

    fn run_writer(&self, mut writer: UnixStream, closed: Arc<AtomicBool>) {
        let _ = writer.set_write_timeout(Some(WRITE_TIMEOUT));
        let mut last_sent: u64 = 0;
        loop {
            let (sequence, next) = {
                let mut guard = self.state.lock().unwrap();
                loop {
                    // The client hanging up wakes this too, so a writer
                    // whose connection is gone exits instead of parking
                    // here for a publish that may never come.
                    if closed.load(Ordering::SeqCst) {
                        return;
                    }
                    if guard.sequence > last_sent
                        && let Some(latest) = guard.latest.as_ref()
                    {
                        break (guard.sequence, Arc::clone(latest));
                    }
                    guard = self
                        .signal
                        .wait(guard)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                }
            };
            match next.as_ref() {
                Published::Snapshot { line } => {
                    if writer.write_all(line.as_bytes()).is_err() {
                        if !closed.swap(true, Ordering::SeqCst) {
                            self.release();
                        }
                        return;
                    }
                    last_sent = sequence;
                }
                Published::Oversize => {
                    write_error(
                        &writer,
                        "frame_too_large",
                        &format!("snapshot frame exceeds {MAX_FRAME_BYTES} bytes"),
                        false,
                        &self.instance,
                    );
                    if !closed.swap(true, Ordering::SeqCst) {
                        self.release();
                    }
                    return;
                }
            }
        }
    }

    fn release(&self) {
        let mut state = self.state.lock().unwrap();
        state.active = state.active.saturating_sub(1);
    }

    pub fn subscriber_count(&self) -> usize {
        self.state.lock().unwrap().active
    }
}

fn write_error(stream: &UnixStream, code: &str, message: &str, retryable: bool, instance: &str) {
    let line = serde_json::json!({
        "type": "error",
        "protocol": STREAM_PROTOCOL,
        "instance": instance,
        "code": code,
        "message": message,
        "retryable": retryable,
    })
    .to_string()
        + "\n";
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    let mut stream = stream;
    let _ = stream.write_all(line.as_bytes());
}

/// Only the user running the daemon may subscribe.
///
/// `UnixStream::peer_cred` is still unstable, so this goes through rustix
/// rather than the hand-rolled `getsockopt` it replaced.
fn peer_uid_matches(stream: &UnixStream) -> bool {
    rustix::net::sockopt::socket_peercred(stream)
        .is_ok_and(|peer| peer.uid == rustix::process::geteuid())
}

fn generate_instance() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("{:x}-{:x}", std::process::id(), nanos)
}

/// Persistent generic client: one NDJSON frame per line on standard output.
///
/// Reconnects with bounded backoff while the daemon restarts and reports one
/// local connection frame per outage.
pub fn run_subscribe_client() -> ! {
    let mut delay = RECONNECT_MIN;
    let mut reported_outage = false;
    loop {
        match UnixStream::connect(ipc::socket_path()) {
            Ok(stream) => {
                if connect_and_stream(stream, &mut reported_outage) {
                    delay = RECONNECT_MIN;
                }
                report_outage(&mut reported_outage, "daemon socket closed");
            }
            Err(error) => report_outage(
                &mut reported_outage,
                &format!("daemon unreachable: {error}"),
            ),
        }
        thread::sleep(jittered(delay));
        delay = (delay * 5 / 2).min(RECONNECT_MAX);
    }
}

/// Add up to ten percent jitter so many clients do not retry in lockstep.
fn jittered(delay: Duration) -> Duration {
    let tenth = delay / 10;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|now| now.subsec_nanos())
        .unwrap_or_default();
    delay + tenth * (nanos % 100) / 100
}

fn connect_and_stream(stream: UnixStream, reported_outage: &mut bool) -> bool {
    let mut writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(_) => return false,
    };
    let _ = stream.set_read_timeout(None);
    let _ = writer.set_write_timeout(Some(Duration::from_secs(1)));
    if writer.write_all(b"subscribe 1\n").is_err() {
        return false;
    }
    let _ = writer.flush();
    // Keep the write half open. The server now reads one request line, and
    // its EOF watcher treats a read EOF as a disconnected subscriber; closing
    // this half after the request would kill the stream right after hello.
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut delivered = false;
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if !delivered {
                    // A frame from the daemon marks the connection active
                    // again, so the next loss may report a fresh outage.
                    *reported_outage = false;
                    delivered = true;
                }
                print!("{line}");
                let _ = std::io::stdout().flush();
            }
        }
    }
    delivered
}

fn report_outage(reported: &mut bool, reason: &str) {
    if *reported {
        return;
    }
    *reported = true;
    let escaped = reason.replace('\\', "\\\\").replace('"', "\\\"");
    println!(
        "{{\"type\":\"connection\",\"protocol\":{STREAM_PROTOCOL},\"connected\":false,\
         \"reason\":\"{escaped}\"}}"
    );
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;

    fn hub() -> Arc<SubscriptionHub> {
        Arc::new(SubscriptionHub::new())
    }

    /// Attach through a socket pair and return a line reader over the client
    /// half. The reader must outlive every read so buffering is preserved.
    fn attach_pair(hub: &Arc<SubscriptionHub>) -> (UnixStream, impl FnMut() -> String + '_) {
        let (client, server) = UnixStream::pair().unwrap();
        hub.attach(server, STREAM_PROTOCOL);
        let mut reader = BufReader::new(client);
        (reader.get_ref().try_clone().unwrap(), move || {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            line
        })
    }

    #[test]
    fn subscriber_receives_hello_then_newest_snapshot() {
        let hub = hub();
        let (_client, mut read_frame) = attach_pair(&hub);
        hub.publish(9, r#"{"generation":9}"#);
        hub.publish(10, r#"{"generation":10}"#);

        let hello: serde_json::Value = serde_json::from_str(&read_frame()).unwrap();
        assert_eq!(hello["type"], "hello");
        assert_eq!(hello["protocol"], 1);
        assert_eq!(hello["schema"], 1);
        assert!(hello["instance"].is_string());

        // Generations published before the writer ran collapse to the newest.
        let frame: serde_json::Value = serde_json::from_str(&read_frame()).unwrap();
        assert_eq!(frame["type"], "snapshot");
        assert_eq!(frame["generation"], 10);
        assert_eq!(frame["snapshot"]["generation"], 10);
    }

    #[test]
    fn later_generations_replace_older_unsent_frames() {
        let hub = hub();
        let (_client, mut read_frame) = attach_pair(&hub);
        hub.publish(41, r#"{}"#);
        hub.publish(42, r#"{}"#);
        hub.publish(43, r#"{}"#);

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&read_frame()).unwrap()["type"],
            "hello"
        );
        let frame: serde_json::Value = serde_json::from_str(&read_frame()).unwrap();
        assert_eq!(frame["generation"], 43);
    }

    #[test]
    fn unsupported_version_gets_structured_error() {
        let hub = hub();
        let (client, server) = UnixStream::pair().unwrap();
        hub.attach(server, 99);

        let frame: serde_json::Value = serde_json::from_str(&read_one_line(client)).unwrap();
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "unsupported_protocol");
        assert_eq!(frame["retryable"], false);
    }

    #[test]
    fn oversized_snapshot_reports_frame_too_large() {
        let hub = hub();
        let (_client, mut read_frame) = attach_pair(&hub);
        let body = format!("\"{}\"", "x".repeat(MAX_FRAME_BYTES));
        hub.publish(1, &body);

        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&read_frame()).unwrap()["type"],
            "hello"
        );
        let frame: serde_json::Value = serde_json::from_str(&read_frame()).unwrap();
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "frame_too_large");
    }

    #[test]
    fn failed_writes_release_the_subscriber_slot() {
        let hub = hub();
        assert_eq!(hub.subscriber_count(), 0);
        let (mut client, server) = UnixStream::pair().unwrap();
        hub.attach(server, STREAM_PROTOCOL);
        hub.publish(1, r#"{}"#);

        let mut line = String::new();
        BufRead::read_line(&mut BufReader::new(&mut client), &mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["type"],
            "hello"
        );
        // Dropping the only client handle makes further writes fail, which is
        // how a local socket reports a vanished subscriber.
        drop(client);
        for generation in 2.. {
            hub.publish(generation, r#"{}"#);
            if hub.subscriber_count() == 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
            assert!(generation < 100, "subscriber slot was never released");
        }
        assert_eq!(hub.subscriber_count(), 0);
    }

    /// A subscriber that hangs up while the daemon is idle must not keep its
    /// slot. run_writer is blocked on the condvar in that state, so only the
    /// read side can notice.
    #[test]
    fn a_disconnected_subscriber_releases_its_slot_without_a_publish() {
        let hub = hub();
        let (client, server) = UnixStream::pair().unwrap();
        hub.attach(server, STREAM_PROTOCOL);
        assert_eq!(hub.subscriber_count(), 1);

        // The client goes away, and nothing is ever published afterwards.
        drop(client);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while hub.subscriber_count() != 0 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            hub.subscriber_count(),
            0,
            "an idle daemon must still reclaim the slot of a client that hung up"
        );
    }

    fn read_one_line(mut stream: UnixStream) -> String {
        let mut line = String::new();
        BufReader::new(Read::by_ref(&mut stream))
            .read_line(&mut line)
            .unwrap();
        line
    }

    /// Hand-escaping covered backslash and quote only, so a control character
    /// in an error message produced a frame the client could not parse.
    #[test]
    fn error_frames_escape_control_characters() {
        let (client, server) = UnixStream::pair().unwrap();
        write_error(
            &server,
            "bad_request",
            "line one\nline two\ttabbed \u{1} and a \"quote\" and a \\ backslash",
            true,
            "test-instance",
        );
        drop(server);

        let mut raw = String::new();
        {
            let mut client = client;
            client.read_to_string(&mut raw).unwrap();
        }
        let frame: serde_json::Value =
            serde_json::from_str(raw.trim()).expect("error frame must be valid JSON");
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["code"], "bad_request");
        assert_eq!(frame["retryable"], true);
        assert_eq!(
            frame["message"],
            "line one\nline two\ttabbed \u{1} and a \"quote\" and a \\ backslash"
        );
    }

    /// A writer parked waiting for a frame must exit when its client hangs
    /// up. It used to stay parked forever, leaking one thread per connection.
    #[test]
    fn a_disconnected_subscriber_does_not_leave_its_writer_parked() {
        let hub = Arc::new(SubscriptionHub::new());
        let before = thread_count();

        for _ in 0..8 {
            let (client, server) = UnixStream::pair().unwrap();
            hub.attach(server, STREAM_PROTOCOL);
            // Let the writer reach the condvar and park BEFORE hanging up.
            // Dropping immediately lets it see `closed` on its first check,
            // which never exercises the wake and hides the leak.
            thread::sleep(Duration::from_millis(60));
            drop(client);
        }

        // Slots come back...
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while hub.subscriber_count() != 0 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(hub.subscriber_count(), 0);

        // ...and so do the threads, without anything being published.
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while thread_count() > before + 2 && std::time::Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let after = thread_count();
        assert!(
            after <= before + 2,
            "writer threads leaked: {before} before, {after} after eight connect/disconnect cycles"
        );
    }

    /// A title-only change moves content_revision and deliberately leaves
    /// generation alone. Freshness used to key on generation, so the frame was
    /// silently never sent. Read with a timeout so a regression fails rather
    /// than hangs.
    #[test]
    fn a_title_only_publish_reaches_a_subscriber() {
        use std::io::BufRead;

        let hub = Arc::new(SubscriptionHub::new());
        let (client, server) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        hub.attach(server, STREAM_PROTOCOL);

        let mut reader = std::io::BufReader::new(client);
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("hello frame");
        assert!(hello.contains("\"type\":\"hello\""), "{hello}");

        hub.publish(7, r#"{"generation":7,"content_revision":1}"#);
        let mut first = String::new();
        assert!(
            reader.read_line(&mut first).unwrap_or(0) > 0,
            "the first snapshot frame must arrive"
        );

        // Same generation, higher content_revision: the title-only case.
        hub.publish(7, r#"{"generation":7,"content_revision":2}"#);
        let mut second = String::new();
        let read = reader.read_line(&mut second).unwrap_or(0);
        assert!(
            read > 0,
            "a title-only publish at an unchanged generation must still be delivered"
        );
        assert!(
            second.contains("\"content_revision\":2"),
            "expected the newer revision, got: {second}"
        );
    }

    /// Threads in this process, read from /proc.
    fn thread_count() -> usize {
        std::fs::read_dir("/proc/self/task")
            .map(|entries| entries.count())
            .unwrap_or(0)
    }
}
