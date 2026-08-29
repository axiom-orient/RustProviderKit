use std::process::ExitCode;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match rgxamk_native_provider::run(&args).await {
        Ok(response) => {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            if stdout.write_all(&response).is_err() || stdout.flush().is_err() {
                return emit_error(&rgxamk_native_provider::ProcessError::output_failed());
            }
            ExitCode::SUCCESS
        }
        Err(error) => emit_error(&error),
    }
}

fn emit_error(error: &rgxamk_native_provider::ProcessError) -> ExitCode {
    eprintln!(
        "rgxamk-native-provider error code={} message={}",
        error.code, error.message
    );
    if let Some(line) = error.diagnostic_line() {
        eprintln!("{line}");
    }
    ExitCode::from(1)
}
