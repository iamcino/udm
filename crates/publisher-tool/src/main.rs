//! `udm-publisher-tool` command line: the offline publisher tool of
//! design section 2.5. Thin wrapper over the library; all rules live
//! there.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use udm_publisher_tool::{keygen, sign_manifest, ToolError};

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
