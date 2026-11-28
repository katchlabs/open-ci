# Cache Server Storage Tuning

`open-ci cache` (or `open-ci serve --with-cache`) stores archives under `cache.storage.path`
(default `/var/lib/open-ci-cache/blobs`) and its `SQLite` index at `cache.index_path`. Both are
disposable cache state: losing them only turns restores into misses.

## Ephemeral Local NVMe (Recommended for `local` storage)

Put the whole `/var/lib/open-ci-cache` directory on a local NVMe disk (AWS Instance Store,
GCP Local SSD) formatted **without an ext4 journal** and mounted **`noatime` without `discard`**:

```bash
sudo systemctl stop open-ci-cache.service open-ci.service
sudo infra/scripts/setup-cache-nvme.sh \
  --device /dev/disk/by-id/nvme-Amazon_EC2_NVMe_Instance_Storage_AWS1234567890 \
  --install-unit
sudo systemctl start open-ci-cache.service   # or open-ci.service when using --with-cache
```

The script formats with `mkfs.ext4 -F -O ^has_journal -m 0` and mounts with `-o noatime`.
`--install-unit` installs `open-ci-cache-nvme.service`, which re-runs it on every boot before
`open-ci.service` / `open-ci-cache.service`: it reformats a disk a stop/start wiped, preens a
reused one with `e2fsck -p`, and reformats it when that repair fails. It
only formats a blank disk or one already labelled `open-ci-cache` (`--force` overrides), and refuses
the root disk, partitioned disks, and a non-empty cache directory. Use a stable
`/dev/disk/by-id/...` path because `nvmeXnY` names can change across boots.

Keep the index on the same disk as the blobs so both are wiped together; an index that outlives
its blobs advertises entries whose reads fail.

### Why

| Default `ext4` behavior | Effect on parallel cache saves | Fix |
| --- | --- | --- |
| `data=ordered` journal | One `fsync()` commits the shared journal transaction, which first writes back the dirty data of **every** file in it. A 500-byte manifest `fsync` waited for ~5 GB of unrelated in-flight uploads; 5 concurrent 1 GB saves froze for 9.75 s and unblocked on the same millisecond. | `open-ci-cache` never fsyncs (enforced by `scripts/verify.sh` Gate 1); `-O ^has_journal` removes the shared transaction entirely. |
| `discard` mount option | Synchronous online TRIM on every unlink/commit-time block free stalls writers while the drive erases. | Mount without `discard`. Instance-store disks are scrubbed by the cloud on release. |
| `atime` updates | Every restore read dirties inode metadata. | `noatime`. |
| 5% reserved blocks | Root-reserved space is wasted on a single-purpose scratch disk. | `-m 0`. |

### Durability Model

Blobs, staged blocks, and `.manifest` files are published with write + flush + atomic `rename`,
so readers see either nothing or a complete file while the host is up. Nothing is `fsync`ed. After
a power loss a torn manifest parses as a miss, and a torn block fails the restore's archive
checksum, so `actions/cache` warns and the job falls back to a cold build. The `SQLite` index runs `journal_mode = WAL` with
`synchronous = NORMAL` on the same trade.

## RAM Hot Tier

`cache.hot_cache_bytes` enables `HotMemoryBlobStore`, a bounded LRU over the disk store. Uploads
are teed into 1 MiB buffers while they stream, so a captured upload never holds both the raw
request frames and a coalesced copy. Committed + staged bytes never exceed the budget, and neither
do in-flight captures. Evicted buffers are freed after the state lock is released, so dropping
gigabytes of chunks never blocks concurrent reads or saves. Proof:
`crates/open-ci-cache/tests/blob_store_concurrency.rs`.
