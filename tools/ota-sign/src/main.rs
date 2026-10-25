//! Make, sign and check signed firmware releases (see `meshcore::release`).
//!
//! ```text
//! ota-sign keygen <key-file>                 new root key (32-byte seed, hex); prints its fingerprint
//! ota-sign fingerprint (--key <file> | --key-env VAR)
//! ota-sign bundle <image> --board heltec-v4 --role repeater --counter 42 --version 0.5.0 -o <out.mcfw>
//! ota-sign sign <file.mcfw> (--key <file> | --key-env VAR)
//! ota-sign verify <file.mcfw> [--root <fingerprint>]...
//! ota-sign extract <file.mcfw> -o <image>
//! ```
//!
//! A `.mcfw` file is the envelope (manifest and signatures) followed by the image. `verify` with
//! `--root` exits with an error unless a pinned root signed the release and the image matches.
//! The release CI keeps the root key's seed in a secret and signs with `--key-env`.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use meshcore::LocalIdentity;
use meshcore::release::{self, Envelope, Fingerprint, Manifest};
use meshcore::util::{from_hex, to_hex};

fn usage() -> ! {
    eprintln!("{}", include_str!("main.rs").lines().skip(3).take(7).map(|l| &l[4.min(l.len())..]).collect::<Vec<_>>().join("\n"));
    std::process::exit(2)
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("ota-sign: {msg}");
    std::process::exit(1)
}

/// `--name value` options and the positional arguments.
struct Args {
    positional: Vec<String>,
    options: Vec<(String, String)>,
}

impl Args {
    fn parse(args: impl Iterator<Item = String>) -> Self {
        let mut a = Args { positional: Vec::new(), options: Vec::new() };
        let mut it = args.peekable();
        while let Some(arg) = it.next() {
            match arg.strip_prefix("--").or_else(|| (arg == "-o").then_some("out")) {
                Some(name) => {
                    let value = it.next().unwrap_or_else(|| fail(format!("--{name} needs a value")));
                    a.options.push((name.to_string(), value));
                }
                None => a.positional.push(arg),
            }
        }
        a
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.options.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    fn need(&self, name: &str) -> &str {
        self.get(name).unwrap_or_else(|| fail(format!("--{name} is required")))
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.options.iter().filter(|(n, _)| n == name).map(|(_, v)| v.as_str()).collect()
    }

    fn file(&self, i: usize) -> &str {
        self.positional.get(i).map(String::as_str).unwrap_or_else(|| usage())
    }
}

fn seed_from_text(text: &str) -> [u8; 32] {
    let hex: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let mut seed = [0u8; 32];
    if hex.len() != 64 || !from_hex(&mut seed, &hex) {
        fail("a key is 64 hex digits (the 32-byte seed)");
    }
    seed
}

/// The signing key from `--key <file>` or `--key-env <VAR>`.
fn key(args: &Args) -> LocalIdentity {
    let text = match (args.get("key"), args.get("key-env")) {
        (Some(path), _) => std::fs::read_to_string(path).unwrap_or_else(|e| fail(format!("{path}: {e}"))),
        (None, Some(var)) => std::env::var(var).unwrap_or_else(|_| fail(format!("environment variable {var} is not set"))),
        (None, None) => fail("--key <file> or --key-env <VAR> is required"),
    };
    LocalIdentity::from_seed(&seed_from_text(&text))
}

fn read(path: &str) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| fail(format!("{path}: {e}")))
}

fn write(path: &str, data: &[u8]) {
    std::fs::write(path, data).unwrap_or_else(|e| fail(format!("{path}: {e}")))
}

/// The envelope and the image of a `.mcfw` file.
fn open(path: &str) -> (Envelope, Vec<u8>) {
    let data = read(path);
    let (envelope, len) = Envelope::parse(&data).unwrap_or_else(|| fail(format!("{path}: not a .mcfw file")));
    (envelope, data[len..].to_vec())
}

fn save(path: &str, envelope: &Envelope, image: &[u8]) {
    write(path, &[envelope.to_bytes(), image.to_vec()].concat())
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    meshcore::crypto::sha256(&mut out, &[data]);
    out
}

fn print_key(id: &LocalIdentity) {
    println!("public key  {}", to_hex(id.pub_key()).to_ascii_lowercase());
    println!("fingerprint {}", release::fingerprint_hex(&release::fingerprint(id.pub_key())));
}

fn show(envelope: &Envelope, image: &[u8], roots: &[Fingerprint]) -> bool {
    let m = &envelope.manifest;
    println!(
        "{}/{} {} #{} issued {}  {} bytes  sha256 {}",
        release::board_name(m.board),
        release::role_name(m.role),
        m.version,
        m.counter,
        m.issued,
        m.image_len,
        to_hex(&m.image_sha256).to_ascii_lowercase()
    );
    let image_ok = image.len() == m.image_len as usize && sha256(image) == m.image_sha256;
    println!("  image: {}", if image_ok { "matches the manifest" } else { "DOES NOT MATCH the manifest" });
    let valid = envelope.valid_signers();
    for s in &envelope.signatures {
        let fp = release::fingerprint(&s.public_key);
        let state = match (valid.contains(&fp), roots.contains(&fp)) {
            (false, _) => "INVALID",
            (true, true) => "valid, pinned root",
            (true, false) => "valid",
        };
        println!("  signed by {}  {state}", release::fingerprint_hex(&fp));
    }
    if envelope.signatures.is_empty() {
        println!("  not signed");
    }
    image_ok && (roots.is_empty() || valid.iter().any(|fp| roots.contains(fp)))
}

fn main() -> ExitCode {
    let mut argv = std::env::args().skip(1);
    let command = argv.next().unwrap_or_else(|| usage());
    let args = Args::parse(argv);
    match command.as_str() {
        "keygen" => {
            let path = args.file(0);
            if std::path::Path::new(path).exists() {
                fail(format!("{path} exists, not overwriting a key"));
            }
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).unwrap_or_else(|e| fail(format!("no randomness: {e}")));
            write(path, format!("{}\n", to_hex(&seed).to_ascii_lowercase()).as_bytes());
            println!("root key written to {path}: keep it secret, devices pin the fingerprint");
            print_key(&LocalIdentity::from_seed(&seed));
        }
        "fingerprint" => print_key(&key(&args)),
        "bundle" => {
            let image = read(args.file(0));
            let board = release::board_by_name(args.need("board")).unwrap_or_else(|| fail("--board: heltec-v4, heltec-v3, xiao-c6 or t-echo"));
            let role = release::role_by_name(args.need("role")).unwrap_or_else(|| fail("--role: repeater or companion"));
            let counter = args.need("counter").parse().unwrap_or_else(|_| fail("--counter: a release number"));
            let version = args.need("version");
            if version.len() > release::VERSION_MAX {
                fail(format!("--version: at most {} bytes", release::VERSION_MAX));
            }
            let issued = match args.get("issued") {
                Some(v) => v.parse().unwrap_or_else(|_| fail("--issued: unix time")),
                None => SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as u32),
            };
            let manifest = Manifest {
                board,
                role,
                flags: 0,
                counter,
                image_len: image.len() as u32,
                image_sha256: sha256(&image),
                issued,
                version: version.into(),
            };
            let envelope = Envelope { manifest, signatures: Vec::new() };
            save(args.need("out"), &envelope, &image);
            show(&envelope, &image, &[]);
        }
        "sign" => {
            let path = args.file(0);
            let (mut envelope, image) = open(path);
            if sha256(&image) != envelope.manifest.image_sha256 {
                fail(format!("{path}: the image does not match its manifest"));
            }
            let id = key(&args);
            let signature = id.sign(&Envelope::signed_message(&envelope.manifest));
            envelope.add_signature(*id.pub_key(), signature);
            save(path, &envelope, &image);
            show(&envelope, &image, &[]);
        }
        "verify" | "show" => {
            let roots: Vec<Fingerprint> = args
                .all("root")
                .iter()
                .map(|r| release::parse_fingerprints(r).and_then(|v| v.first().copied()).unwrap_or_else(|| fail("--root: 64 hex digits")))
                .collect();
            let (envelope, image) = open(args.file(0));
            if !show(&envelope, &image, &roots) {
                return ExitCode::FAILURE;
            }
        }
        "extract" => {
            let (_, image) = open(args.file(0));
            write(args.need("out"), &image);
        }
        _ => usage(),
    }
    ExitCode::SUCCESS
}
