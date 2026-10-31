# scripts/windows

`verify.ps1` runs the Windows verification runbook (W1-W8 in `.overall_plan.md` section 6.3) and writes
a results table in the plan's report-back format.

```powershell
# All steps, from the repo root
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\verify.ps1
# Some steps only
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\windows\verify.ps1 -Step W5,W6,W7,W8
```

From WSL: `powershell.exe -NoProfile -ExecutionPolicy Bypass -File C:\<path to repo>\scripts\windows\verify.ps1`.

| Step | What | Needs |
|---|---|---|
| W1 | `cargo build --workspace --locked`, no warnings from the backends or `win-sys` | - |
| W2 | `cargo test --workspace --all-targets --locked --no-fail-fast` | - |
| W3 | `cargo clippy ... -D warnings` and `cargo fmt --all --check` | - |
| W4 | `mft_vs_walk` ignored test: `$MFT` tree equals the walker's | elevated |
| W5 | scan a scratch tree with and without `--no-mft`; reports match; `lookup` finds the partner | elevated |
| W6 | non-elevated scan prints one `note:` fallback line and matches W5 | W5 in the same run; from an elevated shell it uses `runas /trustlevel:0x20000` |
| W7 | five rescans into W5's DB (walker, MFT, cache rebuild, cache twice) keep the report | elevated, W5 |
| W8 | Ctrl-C: single during the MFT read (exit 130, `cancelling`, no `.tmp`, resume matches W5), double (exit 130 within 2 s), and with a cache present; latency of each case is recorded. Uses `DEDUPE_CACHE_DIR=<OutDir>\w8cache`; SKIP when the console attach is refused | elevated, W5 |

Steps that cannot run are reported as SKIP; the exit code is 1 only when a step FAILs.

- Builds go to `target-win\` (gitignored), so a WSL or Linux `target\` is never touched.
- `-TestRoot` (default `C:\dd_test`) is deleted and recreated. The script refuses a non-empty folder it did
  not create (it leaves a `.dedupe-verify` marker file).
- Logs, databases and `results.md` go to `target-win\verify\<timestamp>\` (or `-OutDir`).
- Needs the pinned toolchain (`rust-toolchain.toml`, MSVC) and Visual Studio Build Tools with the C++
  workload (bundled SQLite and `blake3` compile C). Windows PowerShell 5.1 is enough.
