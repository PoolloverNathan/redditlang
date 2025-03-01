use crate::{
    errors::error,
    llvm::{llvm, Compiler},
};
use clap::{Parser, Subcommand};
use colored::Colorize;
use git::clone_else_pull;
use inkwell::{
    context::Context,
    passes::PassManager,
    targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine},
    AddressSpace, OptimizationLevel,
};
use parser::{parse, Tree};
use pest::Parser as PestParser;
use pest_derive::Parser as PestParser;
use project::Project;
use std::{
    env, fs,
    hash::Hash,
    path::{Path, PathBuf},
    process::Command,
};

pub mod errors;
pub mod from_pair;
pub mod git;
pub mod llvm;
pub mod logger;
pub mod parser;
pub mod project;
pub mod utils;

#[derive(PestParser)]
#[grammar = "../grammar.pest"]
struct RLParser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Builds a program
    Cook {
        /// Enables release mode, longer build but more optimizations.
        #[arg(short, long)]
        release: bool,
    },
    // /// Builds and runs a program
    // Brwww {
    //     /// Enables release mode, longer build but more optimizations.
    //     #[arg(short, long)]
    //     release: bool,
    // },
    /// Creates a new walter project
    New {
        #[arg(short, long)]
        name: String,
    },
}

fn get_project() -> Project {
    match Project::from_path(env::current_dir().unwrap().as_path()) {
        Some(x) => x,
        None => {
            log::error!("No valid {} found.", "walter.yml".bold());
            std::process::exit(1);
        }
    }
}

const STDLIB_URL: &str = "https://github.com/elijah629/redditlang-std";

fn build_libstd() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let walter_dir = dirs::home_dir().unwrap().join(".walter");
    let std_dir = walter_dir.join("stdlib");

    fs::create_dir_all(&walter_dir)?;

    // Make sure libstd is up to date
    clone_else_pull(STDLIB_URL, &std_dir, "main").expect("Failed to clone libstd repo");

    Command::new("cargo")
        .arg("build")
        .arg("--release")
        .current_dir(&std_dir)
        .output()?;

    fs::rename(
        &std_dir.join("target/release/libstd.a"),
        &std_dir.join("libstd.a"),
    )?;

    Command::new("cargo")
        .arg("clean")
        .current_dir(&std_dir)
        .output()?;

    Ok(std_dir.join("libstd.a"))
}

fn main() {
    let args = Args::parse();
    logger::init().unwrap();

    match args.command {
        Commands::Cook { release } => {
            let project = get_project();
            let std_path = match build_libstd() {
                Ok(x) => x,
                Err(x) => {
                    log::error!("Error building libstd: {:?}", x);
                    std::process::exit(1);
                }
            };

            let project_dir = Path::new(&project.path);
            let build_dir =
                project_dir
                    .join("build")
                    .join(if release { "release" } else { "debug" });
            let src_dir = project_dir.join("src");
            let main_file = src_dir.join("main.rl");
            let main_file = fs::read_to_string(&main_file).unwrap();

            fs::create_dir_all(&build_dir).unwrap();

            log::info!("Lexing/Parsing");

            let tree = parse_file(&main_file);

            let context = Context::create();
            let module = context.create_module("main");
            let builder = context.create_builder();

            let fpm = PassManager::create(&module);

            // TODO: Add more passes for better optimization
            fpm.add_instruction_combining_pass();
            fpm.add_reassociate_pass();
            fpm.add_gvn_pass();
            fpm.add_cfg_simplification_pass();
            fpm.add_basic_alias_analysis_pass();
            fpm.add_promote_memory_to_register_pass();
            fpm.add_instruction_combining_pass();
            fpm.add_reassociate_pass();

            fpm.initialize();

            let compiler = &Compiler {
                context: &context,
                module,
                builder,
                fpm,
            };

            // Add libstd functions

            let println_type = compiler.context.void_type().fn_type(
                &[compiler
                    .context
                    .i8_type()
                    .ptr_type(AddressSpace::default())
                    .into()],
                false,
            );
            compiler
                .module
                .add_function("coitusinterruptus", println_type, None);

            let main_type = compiler.context.i32_type().fn_type(&[], false);
            let main_fn = compiler.module.add_function("main", main_type, None);

            let entry_basic_block = compiler.context.append_basic_block(main_fn, "entry");
            compiler.builder.position_at_end(entry_basic_block);

            log::info!("Converting AST to LLVM");

            llvm(&compiler, &tree);

            compiler
                .builder
                .build_return(Some(&compiler.context.i32_type().const_zero()));

            match compiler.module.verify() {
                Err(x) => {
                    log::error!("Module verification failed: {}", x.to_string());
                    std::process::exit(1);
                }
                _ => {}
            };

            log::info!("Compiling");

            Target::initialize_x86(&InitializationConfig::default());

            let opt = OptimizationLevel::Aggressive;
            let reloc = RelocMode::PIC;
            let model = CodeModel::Default;

            let object_path = &build_dir.join(format!("{}.redd.it.o", project.config.name));

            let target = Target::from_name("x86-64").unwrap();
            let target_triple = &TargetMachine::get_default_triple();
            let target_machine = target
                .create_target_machine(target_triple, "x86-64", "+avx2", opt, reloc, model)
                .unwrap();

            target_machine
                .write_to_file(
                    &compiler.module,
                    inkwell::targets::FileType::Object,
                    &object_path,
                )
                .unwrap();

            log::info!("Linking");

            let target_str = target_triple.as_str().to_str().unwrap();

            let compiler = cc::Build::new()
                .target(&target_str)
                .out_dir(&build_dir)
                .opt_level(if release { 3 } else { 0 })
                .host(&target_str)
                .cargo_metadata(false)
                .get_compiler();

            let output_file = build_dir.join(&project.config.name);
            let output_file = output_file.to_str().unwrap();

            compiler
                .to_command()
                .arg(&object_path)
                .arg(std_path) // Could add nostd option that removes this
                .args(["-o", output_file])
                .spawn()
                .unwrap();

            log::info!("Done! Executable is avalible at {}", output_file.bold());
        }
        Commands::New { name } => todo!(),
    }
}

fn parse_file(file: &str) -> Tree {
    match RLParser::parse(Rule::Program, file) {
        Ok(x) => parse(x),
        Err(x) => error(x),
    }
}
