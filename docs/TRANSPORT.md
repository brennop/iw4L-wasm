# Transport budgets

Both client and relay validate these environment settings before connecting or accepting work.
Durations are monotonic milliseconds, limited to 1–600000.

| Setting | Default | Owner/outcome |
| --- | ---: | --- |
| `IW4L_NET_RESPONSE_MS` | 20000 | Correlated request; timeout has unknown remote outcome |
| `IW4L_NET_PROGRESS_MS` | 5000 | Useful dedicated-stream progress/acknowledgement |
| `IW4L_NET_TRANSFER_MS` | 90000 | Total dedicated transfer occupation |
| `IW4L_NET_QUEUE_AGE_MS` | 20000 | Queued command/control/bootstrap/reliable data |
| `IW4L_NET_CONNECT_MS` | 60000 | Connection attempt including retries |
| `IW4L_NET_SHUTDOWN_MS` | 500 | Shutdown drain |
| `IW4L_NET_LOADING_MS` | 120000 | Authority loading milestone |
| `IW4L_NET_SILENCE_MS` | 60000 | Authority progress silence |
| `IW4L_NET_ADMISSION_MS` | 60000 | Peer admission milestone |
| `IW4L_NET_QUEUE_ITEMS` | 32 | Control/pending requests; range 1–1024 |
| `IW4L_NET_QUEUE_BYTES` | 8388608 | Purpose queue byte budget; range 262144–67108864 |

Progress must not exceed the total transfer budget. Invalid settings refuse transport startup.
Keepalive and unrelated traffic never renew a request deadline.
Shared reliable framing belongs to the connection reader; dedicated streams stop/reset on failure.
Replaceable gameplay slots retain their oldest unsent age across replacement.
Reliable queues refuse overload and fail the connection visibly on expiry.
Bootstrap streams also retain the protocol payload and concurrency limits.
Both endpoints use Quinn’s default congestion controller (CUBIC).
The application never disables its congestion window after a stall.

Accepted control requests reserve bounded storage for a correlated terminal receipt until the bridge consumes it.
Connection closure resolves every remaining waiter with an explicit unknown remote outcome.
Local commands can expire before connection establishment; those commands have not been sent.
