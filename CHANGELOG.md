# 0.2.1

## Fixes

* Data race when dropping `SendValue`
  (https://github.com/Thomasdezeeuw/inbox/commit/2f62c1efbeda079f9eae050d04d42462b9723677).
* Data race when dropping `Join`
  (https://github.com/Thomasdezeeuw/inbox/commit/798771781ffbef24bbbd969e699db848a90f50ea).

# 0.2.0

## Changed

* **BREAKING**: `oneshot::Receiver::try_recv` now only returns the message, not a
  reset receiver.
  (https://github.com/Thomasdezeeuw/inbox/commit/2dd49a96e55e97e66a6634eab92cb81764c8d0cd).

## Fixes

* `oneshot::Receiver::try_reset` no only resets the if the Sender is fully
  dropped, not still in-progress of dropping.
  (https://github.com/Thomasdezeeuw/inbox/commit/37dd066cdcfa56599c9fcbd06b83ce39d449aeca).
* Fixes a data-race in the oneshot channel, where a reset receiver (returned by
  `oneshot::Receiver::try_reset`) would attempt to free the channel twice.
  (https://github.com/Thomasdezeeuw/inbox/commit/2dd49a96e55e97e66a6634eab92cb81764c8d0cd).

# 0.1.3

## Added

* `Sender::join`, waits until the other side is disconnected
  (https://github.com/Thomasdezeeuw/inbox/commit/31db1d9587e307600fd7e075c1c1f0ad27c438ea).

# 0.1.2

## Added

* `oneshot::Receiver::register_waker`, allows a `task::Waker` to be registered
  (https://github.com/Thomasdezeeuw/inbox/commit/3a711032d789e4652f4ee4d193e0ecaebc1226f4).

# 0.1.1

## Added

* Support of ThreadSanitizer, by using atomic loads instead of fences when the
  thread sanitizer is enabled
  (https://github.com/Thomasdezeeuw/inbox/commit/dc5202e0d621856403a125dcef5bf33e9477d2c4).

## Changed

* Optimised `oneshot::Receiver::try_reset` by dropping the message in place on
  the heap.
  (https://github.com/Thomasdezeeuw/inbox/commit/5a5b5421869e152c18436d56470dd4c4619317e8).

## Fixes

* Data race when dropping `oneshot::Sender`/`Receiver`
  (https://github.com/Thomasdezeeuw/inbox/commit/96ac5a09e2161aebf3a63ff099272828ba961492).
* Possible data race in oneshot channel
  (https://github.com/Thomasdezeeuw/inbox/commit/d40f7db267aabffc446ff6e9860f03f0fc6aa92d).

To catch these kind of problems we now run the address, leak, memory and thread
sanitizers on the CI, in addition to Miri (which we already ran)
(https://github.com/Thomasdezeeuw/inbox/commit/b45ce7ebac8b2926b07a3670276d5548a0426e8a).

# 0.1.0

Initial release.
