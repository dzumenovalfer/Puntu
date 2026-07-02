//! Offline helper to inspect the bundled n-gram models — handy for tuning thresholds.
//!
//! Usage: `puntu-train <word> [word ...]`
//! Prints each word's Russian/English mean trigram log-prob and their delta.

use puntu::detect::Models;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: puntu-train <word> [word ...]");
        std::process::exit(2);
    }
    let models = Models::builtin();
    println!("{:<20} {:>8} {:>8} {:>10}", "word", "ru", "en", "en-ru");
    for w in args {
        let ru = models.ru.score(&w);
        let en = models.en.score(&w);
        println!("{:<20} {:>8.3} {:>8.3} {:>10.3}", w, ru, en, en - ru);
    }
}
