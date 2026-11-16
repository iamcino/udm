//! `udm-publisher-tool` command line: the offline publisher tool of
//! design section 2.5. Thin wrapper over the library; all rules live
//! there.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use udm_publisher_tool::{keygen, sign_manifest, sign_revocation, sign_rotation, ToolError};

const HELP: &str = "\
udm-publisher-tool — offline publisher tool for the udm authenticated updater
(design section 2.5). It runs offline, by hand, at release time. It has no
network code and reads and writes local files only.

TEST KEYS ONLY in development and tests: every key generated here is a fresh
TEST keypair. The real publisher and recovery keys are owner-custody offline
only and are never generated, stored, or used by this tool's tests.

USAGE:
    udm-publisher-tool keygen --out <dir>
    udm-publisher-tool sign-manifest --manifest <path> --key <keyfile> \\
        --previous-manifest <path> --payload <zip>
    udm-publisher-tool sign-rotation --key <keyfile> --successor-pub <pubfile> \\
        --out <path> --announced-epoch <n> --cutover-epoch <n> \\
        --cutover-at <unix> --window-until <unix>
    udm-publisher-tool sign-revocation --key <keyfile> --key-id <pin> \\
        --out <path>

keygen --out <dir>
    Generate one fresh Ed25519 TEST keypair into <dir>:
      publisher.key   private key: the 32-byte seed as 64 lowercase hex
                      characters. Created owner-only where the platform
                      supports it. Never printed anywhere.
      publisher.pub   public key: 32 raw bytes as 64 lowercase hex characters.
    Prints the pin (SHA-256 of the public key, lowercase hex, section 2.1)
    to stdout for out-of-band comparison. Refuses to overwrite existing
    key files in <dir>.

sign-manifest --manifest <path> --key <keyfile> --previous-manifest <path>
              --payload <zip>
    Sign the manifest at <path> in place with the single key in <keyfile>:
      - manifest_epoch becomes the previous published manifest's epoch + 1.
        --previous-manifest is required; omitting it, or a missing or
        unparsable file, refuses and writes nothing. This tool never sets
        epoch 1; the first publish is the operator's release process.
      - issued_at becomes now (Unix seconds); valid_until becomes
        issued_at + 14 days, overwriting the input window.
      - files[] is rebuilt from the payload zip: one entry per archive
        file (directory entries skipped), each with its SHA-256 and size.
      - every urls entry of the input manifest must be HTTPS and carry a
        sha256 query parameter equal to the whole-zip SHA-256.
      - the output carries exactly one signature, whose key_id is the
        signing key's pin. One --key, one signature.
    Any refusal writes nothing and exits non-zero.

sign-rotation --key <keyfile> --successor-pub <pubfile> --out <path>
              --announced-epoch <n> --cutover-epoch <n>
              --cutover-at <unix> --window-until <unix>
    Write one rotation record to <out>, signed by the current publisher
    key in <keyfile>, naming the successor public key in <pubfile> (the
    keygen publisher.pub format; the successor pin is computed from the
    key, never taken from a declared pin). issued_at is the clock, not
    a flag. The record carries exactly one signature over the client's
    canonical bytes. Refuses, writing nothing, whenever the client
    verifier would refuse these inputs: cutover less than 30 days after
    issue (exactly 30 days is accepted); cutover-epoch less than
    announced-epoch + 1; announced-epoch 0; window-until at or before
    cutover-at or more than 30 days after it (exactly 30 days is
    accepted); a negative timestamp; the successor equal to the signing
    key or to the compiled recovery key; or the compiled recovery key
    as the signing key. There is no default window: the operator
    supplies --window-until. <out> is never overwritten.

sign-revocation --key <keyfile> --key-id <pin> --out <path>
    Write one revocation record to <out>, signed by the recovery key
    in <keyfile>, revoking the publisher pin <pin>. issued_at is the
    clock, not a flag. The record carries exactly one signature over
    the client's canonical bytes. Refuses, writing nothing, when
    --key-id is not 64 lowercase hex characters, names the signing key
    itself, or names the compiled recovery pin; or when the signing
    key is the compiled publisher key. <out> is never overwritten.
";

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        eprint!("{HELP}");
        return 2;
    };
    match command {
        "keygen" => match parse_flags(&args[1..]) {
            Ok(flags) => cmd_keygen(&flags),
            Err(code) => code,
        },
        "sign-manifest" => match parse_flags(&args[1..]) {
            Ok(flags) => cmd_sign(&flags),
            Err(code) => code,
        },
        "sign-rotation" => match parse_flags(&args[1..]) {
            Ok(flags) => cmd_sign_rotation(&flags),
            Err(code) => code,
        },
        "sign-revocation" => match parse_flags(&args[1..]) {
            Ok(flags) => cmd_sign_revocation(&flags),
            Err(code) => code,
        },
        "--help" | "-h" | "help" => {
            print!("{HELP}");
            0
        }
        other => {
            eprintln!("error: unknown command {other:?}\n\n{HELP}");
            2
        }
    }
}

/// Parse `--name value` pairs. A flag may repeat; callers decide how
/// many values their flag accepts. Returns Err(exit code) on misuse.
fn parse_flags(args: &[String]) -> Result<BTreeMap<String, Vec<String>>, i32> {
    let mut flags: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let Some(name) = arg.strip_prefix("--") else {
            eprintln!("error: unexpected argument {arg:?}; flags look like --name value");
            return Err(2);
        };
        let Some(value) = iter.next() else {
            eprintln!("error: flag --{name} needs a value");
            return Err(2);
        };
        flags
            .entry(name.to_owned())
            .or_default()
            .push(value.clone());
    }
    Ok(flags)
}

/// The single value of a flag that must appear exactly once.
fn one(flags: &BTreeMap<String, Vec<String>>, name: &str) -> Result<Option<String>, i32> {
    match flags.get(name) {
        None => Ok(None),
        Some(values) if values.len() == 1 => Ok(Some(values[0].clone())),
        Some(_) => {
            eprintln!("error: --{name} may be given only once (one --key, one signature)");
            Err(1)
        }
    }
}

fn required(flags: &BTreeMap<String, Vec<String>>, name: &str) -> Result<String, i32> {
    match one(flags, name)? {
        Some(value) => Ok(value),
        None => {
            eprintln!("error: --{name} is required");
            Err(2)
        }
    }
}

fn report(result: Result<String, ToolError>) -> i32 {
    match result {
        Ok(line) => {
            println!("{line}");
            0
        }
        Err(err) => {
            eprintln!("error: {err}");
            1
        }
    }
}

fn cmd_keygen(flags: &BTreeMap<String, Vec<String>>) -> i32 {
    let out = match required(flags, "out") {
        Ok(out) => out,
        Err(code) => return code,
    };
    report(keygen(&PathBuf::from(out)))
}

fn cmd_sign(flags: &BTreeMap<String, Vec<String>>) -> i32 {
    let manifest = match required(flags, "manifest") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let key = match required(flags, "key") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let previous = match one(flags, "previous-manifest") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let payload = match one(flags, "payload") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => elapsed.as_secs() as i64,
        Err(_) => {
            eprintln!("error: system clock is before the Unix epoch");
            return 1;
        }
    };
    let previous_path = previous.map(PathBuf::from);
    let payload_path = payload.map(PathBuf::from);
    let result = sign_manifest(
        &PathBuf::from(manifest),
        &PathBuf::from(key),
        previous_path.as_deref(),
        payload_path.as_deref(),
        now,
    )
    .map(|signed| {
        format!(
            "signed manifest epoch {} with key_id {}",
            signed.manifest_epoch, signed.signatures[0].key_id
        )
    });
    report(result)
}

/// The signing clock: current Unix seconds. A clock set before the
/// epoch is a hard error, as in `sign-manifest`.
fn now_unix() -> Result<i64, i32> {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => Ok(elapsed.as_secs() as i64),
        Err(_) => {
            eprintln!("error: system clock is before the Unix epoch");
            Err(1)
        }
    }
}

fn parse_u64(flags: &BTreeMap<String, Vec<String>>, name: &str) -> Result<u64, i32> {
    let raw = required(flags, name)?;
    raw.parse().map_err(|_| {
        eprintln!("error: --{name} is not a valid non-negative integer: {raw:?}");
        2
    })
}

fn parse_i64(flags: &BTreeMap<String, Vec<String>>, name: &str) -> Result<i64, i32> {
    let raw = required(flags, name)?;
    raw.parse().map_err(|_| {
        eprintln!("error: --{name} is not a valid integer: {raw:?}");
        2
    })
}

fn cmd_sign_rotation(flags: &BTreeMap<String, Vec<String>>) -> i32 {
    let key = match required(flags, "key") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let successor_pub = match required(flags, "successor-pub") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let out = match required(flags, "out") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let announced_epoch = match parse_u64(flags, "announced-epoch") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let cutover_epoch = match parse_u64(flags, "cutover-epoch") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let cutover_at = match parse_i64(flags, "cutover-at") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let window_until = match parse_i64(flags, "window-until") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let now = match now_unix() {
        Ok(value) => value,
        Err(code) => return code,
    };
    let result = sign_rotation(
        &PathBuf::from(key),
        &PathBuf::from(successor_pub),
        &PathBuf::from(out),
        announced_epoch,
        cutover_epoch,
        cutover_at,
        window_until,
        now,
    )
    .map(|record| {
        format!(
            "signed rotation record for successor {} (cutover epoch {})",
            record.successor.pin(),
            record.cutover_epoch
        )
    });
    report(result)
}

fn cmd_sign_revocation(flags: &BTreeMap<String, Vec<String>>) -> i32 {
    let key = match required(flags, "key") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let key_id = match required(flags, "key-id") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let out = match required(flags, "out") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let now = match now_unix() {
        Ok(value) => value,
        Err(code) => return code,
    };
    let result = sign_revocation(&PathBuf::from(key), &key_id, &PathBuf::from(out), now)
        .map(|record| format!("signed revocation record for key_id {}", record.key_id));
    report(result)
}
