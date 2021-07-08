use crate::{api, commands::Args};
pub enum Compilation {
    Success { asm: String },
    Error { stderr: String },
}

#[derive(Debug, serde::Deserialize)]
struct GodboltOutputSegment {
    text: String,
}

#[derive(Debug, serde::Deserialize)]
struct GodboltOutput(Vec<GodboltOutputSegment>);

impl GodboltOutput {
    pub fn full_with_ansi_codes_stripped(&self) -> Result<String, crate::Error> {
        let mut complete_text = String::new();
        for segment in self.0.iter() {
            complete_text.push_str(&segment.text);
            complete_text.push_str("\n");
        }
        Ok(String::from_utf8(strip_ansi_escapes::strip(
            complete_text.trim(),
        )?)?)
    }
}

#[derive(Debug, serde::Deserialize)]
struct GodboltResponse {
    code: u8,
    stdout: GodboltOutput,
    stderr: GodboltOutput,
    asm: GodboltOutput,
}

pub fn help(args: Args) -> Result<(), crate::Error> {
    let message = "Compile Rust code using <https://rust.godbolt.org/>. Full optimizations are applied unless overriden.
```?godbolt flags={} rustc={} ``\u{200B}`code``\u{200B}` ```
Optional arguments:
    \tflags: flags to pass to rustc invocation. Defaults to \"-Copt-level=3 --edition=2018\".
    \trustc: compiler version to invoke. Defaults to `nightly`. Possible values: `nightly`, `beta` or full version like `1.45.2`.
    ";

    api::send_reply(&args, &message)?;
    Ok(())
}

/// Compile a given Rust source code file on Godbolt using the latest nightly compiler with
/// full optimizations (-O3) by default
/// Returns a multiline string with the pretty printed assembly
pub fn compile_rust_source(
    http: &reqwest::blocking::Client,
    source_code: &str,
    flags: &str,
    rustc: &str,
) -> Result<Compilation, crate::Error> {
    let cv = rustc_to_godbolt(rustc);
    let cv = match cv {
        Ok(c) => c,
        Err(e) => {
            return Ok(Compilation::Error { stderr: e });
        }
    };
    info!("cv: rustc {}", cv);

    let response: GodboltResponse = http
        .execute(
            http.post(&format!("https://godbolt.org/api/compiler/{}/compile", cv))
                .query(&[("options", &flags)])
                .header(reqwest::header::ACCEPT, "application/json")
                .body(source_code.to_owned())
                .build()?,
        )?
        .json()?;

    info!("raw godbolt response: {:#?}", &response);

    Ok(if response.code == 0 {
        Compilation::Success {
            asm: response.asm.full_with_ansi_codes_stripped()?,
        }
    } else {
        Compilation::Error {
            stderr: response.stderr.full_with_ansi_codes_stripped()?,
        }
    })
}

// converts a rustc version number to a godbolt compiler id
fn rustc_to_godbolt(rustc_version: &str) -> Result<String, String> {
    match rustc_version {
        "beta" => Ok("beta".to_string()),
        "nightly" => Ok("nightly".to_string()),
        // this heuristic is barebones but catches most obviously wrong things
        // it doesn't know anything about valid rustc versions
        ver if ver.contains('.') && !ver.contains(|c: char| c.is_alphabetic()) => {
            let mut godbolt_version = "r".to_string();
            for segment in ver.split('.') {
                godbolt_version.push_str(segment);
            }
            Ok(godbolt_version)
        }
        other => Err(format!("invalid rustc version: `{}`", other)),
    }
}
