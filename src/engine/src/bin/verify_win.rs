use std::env;
use std::fs;
use std::time::Instant;
use engine::process_cases_json;
use engine::text::slugify_vec;

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--test-slugify") {
        // Test all Unicode scalar values against Node 22 via stdin/stdout JSON or hex lines on stdin.
        use std::io::{BufRead, BufWriter, Write};
        let stdin = std::io::stdin();
        let stdout = std::io::stdout();
        let mut out = BufWriter::new(stdout.lock());
        for line in stdin.lock().lines() {
            let l = line.unwrap();
            let slug = slugify_vec(l.as_bytes());
            out.write_all(&slug).unwrap();
            out.write_all(b"\n").unwrap();
        }
        out.flush().unwrap();
        return;
    }

    if args.get(1).map(|s| s.as_str()) == Some("--dump-all-cp") {
        // Check single-codepoint slugify for 0..=0x10FFFF written as `cp_hex:slug\n` for diffing against Node.
        use std::io::{BufWriter, Write};
        let stdout = std::io::stdout();
        let mut out = BufWriter::new(stdout.lock());
        let mut buf = [0u8; 4];
        for cp in 0u32..=0x10FFFF {
            if let Some(ch) = char::from_u32(cp) {
                let s = ch.encode_utf8(&mut buf);
                let slug = slugify_vec(s.as_bytes());
                write!(out, "{:x}:", cp).unwrap();
                out.write_all(&slug).unwrap();
                out.write_all(b"\n").unwrap();
            }
        }
        out.flush().unwrap();
        return;
    }

    let in_path = args.get(1).expect("usage: verify_win <cases.json> <answers.json>");
    let out_path = args.get(2).expect("usage: verify_win <cases.json> <answers.json>");
    let data = fs::read(in_path).unwrap();
    let t0 = Instant::now();
    let ans = process_cases_json(&data).expect("failed to process cases.json");
    let elapsed = t0.elapsed();
    fs::write(out_path, &ans).unwrap();
    eprintln!(
        "verify_win: input={}B output={}B avx2={} elapsed_us={}",
        data.len(),
        ans.len(),
        engine::simd::has_avx2(),
        elapsed.as_micros()
    );
}
