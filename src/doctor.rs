use anyhow::Result;

pub fn find_aria2() -> Option<String> {
    which::which("aria2c")
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

pub fn run_doctor() -> Result<i32> {
    println!("Environment check:");
    println!("- OS: {}", std::env::consts::OS);
    println!("- ARCH: {}", std::env::consts::ARCH);

    match find_aria2() {
        Some(path) => {
            println!("- aria2c: found ({})", path);
            println!("Status: OK");
            Ok(0)
        }
        None => {
            println!("- aria2c: missing");
            println!("Install: brew install aria2");
            println!("Status: FAIL");
            Ok(2)
        }
    }
}
