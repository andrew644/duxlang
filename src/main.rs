mod lexer;

use lexer::lexer::lex;
use std::env;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;

fn read_file(file_path: &String) {
    let path = Path::new(file_path);
    let display = path.display();

    let mut file = match File::open(&path) {
        Err(why) => panic!("Couldn't open {display}: {why}"),
        Ok(file) => file,
    };

    let mut code = String::new();
    match file.read_to_string(&mut code) {
        Err(why) => panic!("Couldn't read {display}: {why}"),
        Ok(_) => print!("{display} contains:\n{code}"),
    }

    lex(code);
}

fn main() {
    let args: Vec<String> = env::args().collect();

    match args.len() {
        2 => {
            let file = &args[1];
            println!("Compiling {file}");
            read_file(file);
        }
        _ => {
            let binary_name = &args[0];
            println!("Usage: {binary_name} file.dux");
        }
    }
}
