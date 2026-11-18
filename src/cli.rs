//! Command line parsing: the last of the three `Config` layers, applied on top
//! of the defaults and `~/.config/launchr.json`.

use crate::config::{self, Config};

/// Radius `-b` picks when it is given no number of its own.
pub const DEFAULT_BLUR: u32 = 32;

const HELP: &str = "launchr — Wayland application launcher\n\n\
     Usage: launchr [options]\n\n\
     Options:\n  \
     -l, --lines N     result rows to show (default 6)\n  \
     -w, --width N     panel width in pixels (default 680)\n  \
     -p, --prompt TEXT placeholder text (default: none)\n  \
     -q, --query TEXT  start with the input prefilled\n  \
     -b, --blur [N]    blur the desktop behind the launcher,\n  \
     \x20                 radius N in pixels (default 32, off\n  \
     \x20                 unless asked for; costs a screenshot)\n  \
     -d, --dim F       backdrop dim, 0.0 to 1.0 (default 0.75)\n  \
     \x20   --no-blur     no blur; the default\n  \
     -h, --help        show this help\n\n\
     Colors, font size, list length, dim, blur and a list of\n\
     directly launchable AppImages can also be set in\n\
     ~/.config/launchr.json; flags above override it.";

/// One argument split into its flag and, for `--flag=value`, the value
/// written into it. Only long options take that form: `-l=5` stays whole,
/// and is rejected as an unknown argument.
fn split_attached(arg: String) -> (String, Option<String>) {
    match arg.split_once('=') {
        Some((flag, value)) if flag.starts_with("--") => (flag.to_owned(), Some(value.to_owned())),
        _ => (arg, None),
    }
}

/// The flag's value: the one written into it, else the next argument.
fn flag_value(
    flag: &str,
    attached: Option<String>,
    args: &mut impl Iterator<Item = String>,
) -> Result<String, String> {
    attached.or_else(|| args.next()).ok_or_else(|| format!("{flag} needs a value"))
}

fn parse_number<T: std::str::FromStr>(flag: &str, value: String) -> Result<T, String> {
    value.parse().map_err(|_| format!("{flag} expects a number"))
}

fn no_value(flag: &str, attached: Option<String>) -> Result<(), String> {
    match attached {
        Some(_) => Err(format!("{flag} takes no value")),
        None => Ok(()),
    }
}

/// `base` is the built-in defaults with `~/.config/launchr.json` already
/// applied; any flag here overrides it further. `Ok(None)` means `--help` was
/// printed and no window is wanted. Every flag that takes a value takes it
/// either as the next argument or written in, as `--lines=9`.
pub fn parse_args<I>(base: Config, args: I) -> Result<Option<Config>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut config = base;
    let mut args = args.into_iter().peekable();
    while let Some(arg) = args.next() {
        let (flag, attached) = split_attached(arg);
        match flag.as_str() {
            "-h" | "--help" => {
                no_value(&flag, attached)?;
                println!("{HELP}");
                return Ok(None);
            }
            "-l" | "--lines" => {
                let lines: usize = parse_number(&flag, flag_value(&flag, attached, &mut args)?)?;
                config.lines = lines.clamp(*config::LINES.start(), *config::LINES.end());
            }
            "-w" | "--width" => {
                let width: i32 = parse_number(&flag, flag_value(&flag, attached, &mut args)?)?;
                config.width = width.clamp(*config::WIDTH.start(), *config::WIDTH.end());
            }
            "-p" | "--prompt" => config.placeholder = flag_value(&flag, attached, &mut args)?,
            "-q" | "--query" => config.query = flag_value(&flag, attached, &mut args)?,
            "-b" | "--blur" => {
                // A bare -b means "blur, you pick the radius"; a number after
                // it sets one. Anything else is the next option, left alone —
                // but a value written in has to be a number.
                let radius = match attached {
                    Some(value) => parse_number(&flag, value)?,
                    None => match args.peek().and_then(|next| next.parse::<u32>().ok()) {
                        Some(radius) => {
                            args.next();
                            radius
                        }
                        None => DEFAULT_BLUR,
                    },
                };
                config.blur = radius.min(config::MAX_BLUR);
            }
            "--no-blur" => {
                no_value(&flag, attached)?;
                config.blur = 0;
            }
            "-d" | "--dim" => {
                // `"nan".parse::<f64>()` succeeds and `f64::clamp` hands NaN
                // straight back, which would reach the stylesheet as
                // `rgba(r, g, b, NaN)` and take the whole scrim out with it.
                let dim: f64 = parse_number(&flag, flag_value(&flag, attached, &mut args)?)?;
                if !dim.is_finite() {
                    return Err(format!("{flag} expects a number"));
                }
                config.dim = dim.clamp(*config::DIM.start(), *config::DIM.end());
            }
            _ => return Err(format!("unknown argument: {flag}")),
        }
    }
    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn parse(list: &[&str]) -> Config {
        parse_args(Config::default(), args(list))
            .expect("should parse")
            .expect("should not be --help")
    }

    #[test]
    fn flags_override_the_defaults() {
        let config = parse(&["-l", "9", "-w", "900", "-p", "run:", "-q", "fire", "-d", "0.5"]);
        assert_eq!(config.lines, 9);
        assert_eq!(config.width, 900);
        assert_eq!(config.placeholder, "run:");
        assert_eq!(config.query, "fire");
        assert_eq!(config.dim, 0.5);
    }

    #[test]
    fn flags_override_the_file_layer_they_are_given() {
        let base = Config { lines: 3, blur: 12, ..Config::default() };
        let config = parse_args(base, args(&["-l", "7"])).unwrap().unwrap();
        assert_eq!(config.lines, 7);
        // Untouched by a flag, so the value handed in survives.
        assert_eq!(config.blur, 12);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let config = parse(&["-l", "500", "-w", "10", "-d", "4", "-b", "9000"]);
        assert_eq!(config.lines, 50);
        assert_eq!(config.width, 240);
        assert_eq!(config.dim, 1.0);
        assert_eq!(config.blur, 200);
    }

    #[test]
    fn blur_takes_an_optional_radius() {
        assert_eq!(parse(&["-b"]).blur, DEFAULT_BLUR);
        assert_eq!(parse(&["-b", "12"]).blur, 12);
        // A following option must not be eaten as the radius.
        let config = parse(&["-b", "-l", "4"]);
        assert_eq!(config.blur, DEFAULT_BLUR);
        assert_eq!(config.lines, 4);
        // Last flag wins, in both directions.
        assert_eq!(parse(&["-b", "20", "--no-blur"]).blur, 0);
        assert_eq!(parse(&["--no-blur", "-b", "20"]).blur, 20);
    }

    #[test]
    fn bad_arguments_are_rejected() {
        assert!(parse_args(Config::default(), args(&["--nope"])).is_err());
        assert!(parse_args(Config::default(), args(&["-l"])).is_err());
        assert!(parse_args(Config::default(), args(&["-l", "many"])).is_err());
        // Parses as a float and survives `clamp`, so it needs its own check —
        // it would otherwise reach the stylesheet as `rgba(..., NaN)`.
        assert!(parse_args(Config::default(), args(&["-d", "nan"])).is_err());
        assert!(parse_args(Config::default(), args(&["-d", "inf"])).is_err());
        assert!(parse(&["-d", "0.25"]).dim.is_finite());
    }

    #[test]
    fn a_long_flag_takes_its_value_written_in() {
        let config = parse(&["--lines=9", "--prompt=a=b", "--blur=12", "--dim=0.5"]);
        assert_eq!(config.lines, 9);
        // Split at the first `=` only.
        assert_eq!(config.placeholder, "a=b");
        assert_eq!(config.blur, 12);
        assert_eq!(config.dim, 0.5);
        assert_eq!(parse(&["--query="]).query, "");
        // A value that looks like a flag is still a value.
        assert_eq!(parse(&["-q", "--lines=3"]).query, "--lines=3");

        let rejected = |list: &[&str]| parse_args(Config::default(), args(list)).is_err();
        assert!(rejected(&["--blur=lots"]));
        assert!(rejected(&["--no-blur=1"]));
        assert!(rejected(&["--help=me"]));
        assert!(rejected(&["-l=5"]));
    }

    #[test]
    fn help_asks_for_no_window() {
        assert!(parse_args(Config::default(), args(&["-h"])).unwrap().is_none());
    }
}
