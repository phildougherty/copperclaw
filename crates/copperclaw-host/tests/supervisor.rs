//! M21 S1 integration acceptance: panic-inject a supervised loop in a test
//! host — it resumes within one backoff step, and the `host.status`
//! admin-socket handler reports the restart.
//!
//! Runs on the paused tokio clock (`start_paused`): parking auto-advances
//! the mock clock to the next pending timer, so the 5s backoff step is
//! traversed deterministically with no real waits. The restart interval is
//! asserted from instants recorded inside each loop incarnation.

use copperclaw_cclaw::{Caller, Request, Response, read_response, write_request};
use copperclaw_db::central::CentralDb;
use copperclaw_host::socket::{bind_listener, serve_listener};
use copperclaw_host::supervisor::{BACKOFF_STEPS, Supervisor};
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Poll until `cond` holds. The short mock-clock sleep parks the runtime,
/// and each park auto-advances the paused clock to the next pending timer
/// — so the backoff wait is traversed deterministically. Capped so a bug
/// fails instead of hanging.
async fn wait_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..100_000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("condition not reached under the paused clock");
}

#[tokio::test(start_paused = true)]
async fn panic_injected_loop_resumes_and_status_handler_reports_the_restart() {
    let shutdown = CancellationToken::new();

    // A sweep-like loop: ticks once a second; the FIRST incarnation panics
    // on its first tick (the injected failure), later incarnations keep
    // ticking until shutdown. Incarnation start instants are recorded by
    // the future itself so the restart interval can be asserted exactly.
    let starts: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let ticks = Arc::new(AtomicU32::new(0));
    let mut sup = Supervisor::new(shutdown.clone());
    {
        let starts = Arc::clone(&starts);
        let ticks = Arc::clone(&ticks);
        let sd = shutdown.clone();
        sup.register("sweep", move || {
            let starts = Arc::clone(&starts);
            let ticks = Arc::clone(&ticks);
            let sd = sd.clone();
            async move {
                let n = {
                    let mut s = starts.lock().unwrap();
                    s.push(Instant::now());
                    s.len()
                };
                loop {
                    tokio::select! {
                        () = sd.cancelled() => break,
                        () = tokio::time::sleep(Duration::from_secs(1)) => {
                            ticks.fetch_add(1, Ordering::SeqCst);
                            assert!(n != 1, "injected sweep failure");
                        }
                    }
                }
            }
        });
    }

    // Admin socket wired exactly as boot wires it: the supervisor's status
    // Arc rides the HandlerCtx into the dispatch table.
    let tmp = tempfile::tempdir().unwrap();
    let socket_path = tmp.path().join("cclaw.sock");
    let listener = bind_listener(&socket_path).unwrap();
    let central = CentralDb::open_in_memory().unwrap();
    let status = sup.status();
    let socket_task = tokio::spawn(serve_listener(
        listener,
        socket_path.clone(),
        central,
        tmp.path().to_path_buf(),
        Some(Arc::clone(&status)),
        shutdown.clone(),
    ));

    let driver = sup.run();

    // Sweeping resumes: the second incarnation comes up and ticks.
    wait_until(|| ticks.load(Ordering::SeqCst) >= 2).await;
    {
        let s = starts.lock().unwrap();
        assert_eq!(s.len(), 2, "exactly one restart");
        // Incarnation 1 died at its first tick (1s in); the restart paid
        // exactly one backoff step (5s) on top.
        assert_eq!(
            s[1] - s[0],
            Duration::from_secs(1) + BACKOFF_STEPS[0],
            "loop resumed within one backoff step of the failure"
        );
    }

    // The status handler reports the restart over the real socket.
    let mut stream = UnixStream::connect(&socket_path).await.unwrap();
    let req = Request::Call {
        id: "s1".into(),
        command: "host.status".into(),
        args: json!({}),
        caller: Caller::Host,
    };
    write_request(&mut stream, &req).await.unwrap();
    let resp = read_response(&mut stream).await.unwrap();
    let data = match resp {
        Response::Ok { data, .. } => data,
        Response::Err { error, .. } => panic!("host.status failed: {error:?}"),
    };
    assert_eq!(data["degraded"], false);
    let loops = data["loops"].as_array().unwrap();
    assert_eq!(loops.len(), 1);
    assert_eq!(loops[0]["name"], "sweep");
    assert_eq!(loops[0]["alive"], true);
    assert_eq!(loops[0]["restarts"], 1);
    assert_eq!(loops[0]["degraded"], false);
    assert!(
        loops[0]["last_exit"]
            .as_str()
            .is_some_and(|e| e.contains("injected sweep failure")),
        "last_exit should carry the injected panic: {:?}",
        loops[0]["last_exit"]
    );

    // Shutdown still drains everything.
    shutdown.cancel();
    driver.await.unwrap();
    socket_task.await.unwrap().unwrap();
    assert!(!status.snapshot()[0].alive);
}
