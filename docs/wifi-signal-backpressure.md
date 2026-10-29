# Wi-Fi refresh stalls during signal bursts

The live Wi-Fi listeners previously waited for D-Bus enumeration inside their
signal-consumption loops. zbus 5.15 has bounded signal queues (64 messages by
default). When a queue fills, its socket reader waits for space before reading
more messages, including replies. A listener waiting for one of those replies
cannot drain the queue: the connection stalls.

This is the pattern described in [zbus issue #213](https://github.com/z-galaxy/zbus/issues/213).
It is distinct from an individual AP disappearing during enumeration: that
returns an error, whereas this failure leaves an operation pending indefinitely.

## Reproduction

Run the tests in the development shell (no NetworkManager, GTK display or root
privileges are needed):

```sh
nix develop --command cargo test --locked inline_refresh_stalls -- --nocapture
nix develop --command cargo test --locked burst_ -- --nocapture
```

The private D-Bus service sends 128 AP-removal signals before replying to an
enumeration's `ListConnections` call. The reproduction keeps the signal stream
idle while awaiting enumeration, as the old listener did. It verifies that the
call remains pending, then resumes draining and verifies that the **same call**
completes. A five-second watchdog prevents a regression from hanging the suite.

The regression tests use the production signal drainer and refresh worker with
real D-Bus traffic. During enumeration, the fake service switches visible APs
from `Kirjasto` to `ePhone` and emits 128 signals ahead of its reply. They verify:

- The in-flight snapshot completes despite the burst.
- The burst produces one follow-up snapshot containing `ePhone`.
- Subsequent scan and enumeration calls on the same connection still work.
- Added, removed and device-state signal bursts each behave correctly.

## Fix and scope

Signal callbacks now perform synchronous bookkeeping and notify a shared worker.
They never wait for a debounce timer or D-Bus query. The worker coalesces requests
over a fixed 300 ms window, executes one refresh at a time, and retains at most
one follow-up request while busy. Device-state refreshes retain precedence so
AP churn cannot discard the Home tile and radio-state update. Each listener
starts consuming immediately after subscribing.

This serializes signal-triggered refreshes. It does not serialize every manual,
scan-on-show or tab-navigation refresh, fix the separate `SpecificObject` usage,
or recover a connection already stalled inside an old running executable.
The original AP-disappearance handling remains a separate robustness fix.

The live incident's unread socket backlog and inert refresh/navigation paths
strongly support this mechanism, but the test does not identify which queue was
blocked in that process. On 2026-10-29, the original reporter confirmed that
continued use of the installed patched application resolved their issue.
This complements the automated reproduction and regression tests; the report
does not establish which queue was blocked in the original process.
