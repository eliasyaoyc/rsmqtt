use passwd::HashType;
use structopt::StructOpt;

#[derive(StructOpt)]
struct Options {
    /// hash type
    hash: HashType,

    /// password
    password: String,
}

fn main() {
    let options: Options = Options::from_args();
    println!("{}", options.hash.create_phc(options.password));
}
