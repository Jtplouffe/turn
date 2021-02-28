use std::time::Duration;

use clap::{App, AppSettings, Arg};

use webrtc_rs_turn::auth;

// Outputs username & password according to the
// Long-Term Credential Mechanism (RFC5389-10.2: https://tools.ietf.org/search/rfc5389#section-10.2)
fn main() {
    env_logger::init();

    let mut app = App::new("Long term credential generator")
        .version("0.1.0")
        .author("Rain Liu <yliu@webrtc.rs")
        .about("An example of long term credential generator")
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::SubcommandsNegateReqs)
        .arg(
            Arg::with_name("FULLHELP")
                .help("Prints more detailed help information")
                .long("fullhelp"),
        )
        .arg(
            Arg::with_name("authSecret")
                .required_unless("FULLHELP")
                .takes_value(true)
                .long("authSecret")
                .help("Shared secret for the Long Term Credential Mechanism")
        );

    let matches = app.clone().get_matches();

    if matches.is_present("FULLHELP") {
        app.print_long_help().unwrap();
        std::process::exit(0);
    }

    let auth_secret = matches.value_of("authSecret").unwrap();

    match auth::generate_long_term_credentials(auth_secret, Duration::from_secs(60)) {
        Ok((u, p)) => println!("{}={}", u, p),
        Err(e) => panic!(e),
    }
}