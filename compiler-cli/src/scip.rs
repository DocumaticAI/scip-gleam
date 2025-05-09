/// The `scip` command mostly works by hacking into the compiler used for the language
/// server, which has information on all the symbol information in the codebase (for
/// features like 'go to definition' and 'rename symbol').
use crate::fs::ProjectIO;
use camino::Utf8PathBuf;
use ecow::EcoString;
use gleam_core::{
    Error, Result,
    ast::Definition,
    build::Outcome,
    config::PackageConfig,
    error::{FileIoAction, FileKind},
    io::FileSystemReader,
    language_server::{DownloadDependencies, LspProjectCompiler, MakeLocker},
    paths::ProjectPaths,
};
use hexpm::version::Version;
use scip::types::{
    Descriptor, Index, Occurrence, Package, Symbol, SymbolInformation, SymbolRole,
    descriptor::Suffix, symbol_information::Kind,
};
use scip::{
    symbol::format_symbol,
    types::{Document, Metadata, PositionEncoding, TextEncoding, ToolInfo},
    write_message_to_file,
};

pub fn main(path: Utf8PathBuf, output_file: Utf8PathBuf) -> Result<()> {
    tracing::info!("scip_index_starting");

    eprintln!("Compiling SCIP index");

    let path = path.canonicalize_utf8().map_err(|e| Error::FileIo {
        kind: FileKind::File,
        action: FileIoAction::Canonicalise,
        path: path.clone(),
        err: Some(e.to_string()),
    })?;

    let io = ProjectIO::new();
    let paths = ProjectPaths::new(path.clone());

    let config_path = paths.root_config();
    let toml = io.read(&config_path)?;
    let config: PackageConfig = toml::from_str(&toml).map_err(|e| Error::FileIo {
        action: FileIoAction::Parse,
        kind: FileKind::File,
        path: config_path,
        err: Some(e.to_string()),
    })?;

    let locker = io.make_locker(&paths, config.target)?;

    let manifest = io.download_dependencies(&paths)?;

    let mut compiler = LspProjectCompiler::new(
        manifest.clone(),
        config.clone(),
        paths.clone(),
        ProjectIO::new(),
        locker,
    )?;

    let outcome = compiler.compile();

    match outcome {
        Outcome::TotalFailure(..) => panic!("Failed to parse"),
        _ => (),
    }

    let scip_metadata = Metadata {
        tool_info: Some(ToolInfo {
            name: "scip-gleam".into(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            ..Default::default()
        })
        .into(),
        project_root: path.to_string(),
        text_document_encoding: TextEncoding::UTF8.into(),
        ..Default::default()
    };

    let documents = compiler
        .modules
        .values()
        .map(|module| {
            let symbols = module
                .ast
                .definitions
                .iter()
                .flat_map(|def| {
                    match def {
                        // We don't care about these for now
                        Definition::Import(..)
                        | Definition::TypeAlias(..)
                        | Definition::ModuleConstant(..) => vec![],
                        // TODO: handle custom types
                        Definition::CustomType(type_) => {
                            // A Gleam custom type often looks like this:
                            //
                            // ```
                            // type Wibble {
                            //   Wibble
                            //   Wobble(name: String)
                            // }
                            // ```
                            //
                            // We give the type name the `Type` suffix, and the
                            // constructors get the `Method` suffix.
                            //
                            // ```
                            // type Wibble {
                            //      ^^^^^^ Type
                            //   Wibble
                            //   ^^^^^^ Method
                            //   Wobble(name: String)
                            // }
                            // ```

                            let mut type_symbols = vec![SymbolInformation {
                                symbol: format_symbol(create_scip_symbol(
                                    config.name.to_string(),
                                    config.version.to_string(),
                                    module.name.to_string(),
                                    type_.name.clone(),
                                    Suffix::Type,
                                )),
                                display_name: type_.name.to_string(),
                                documentation: format_doc(def.get_doc()),
                                kind: Kind::Type.into(),
                                ..Default::default()
                            }];

                            let constructor_symbols =
                                type_
                                    .constructors
                                    .iter()
                                    .map(|constructor| SymbolInformation {
                                        symbol: format_symbol(create_scip_symbol(
                                            config.name.to_string(),
                                            config.version.to_string(),
                                            module.name.to_string(),
                                            constructor.name.clone(),
                                            Suffix::Method,
                                        )),
                                        display_name: constructor.name.to_string(),
                                        documentation: format_doc(def.get_doc()),
                                        kind: Kind::Function.into(),
                                        ..Default::default()
                                    });

                            type_symbols.extend(constructor_symbols);
                            type_symbols
                        }
                        Definition::Function(func) => {
                            let function_name = func
                                .name
                                .clone()
                                .map(|(_loc, name)| name)
                                .unwrap_or("todo-unknown".into());

                            vec![SymbolInformation {
                                symbol: format_symbol(create_scip_symbol(
                                    config.name.to_string(),
                                    config.version.to_string(),
                                    module.name.to_string(),
                                    function_name.clone(),
                                    Suffix::Method,
                                )),
                                display_name: function_name.to_string(),
                                documentation: format_doc(def.get_doc()),
                                kind: Kind::Function.into(),
                                ..Default::default()
                            }]
                        }
                    }
                })
                .collect::<Vec<_>>();

            let this_module_source = compiler.sources.get(&module.name).unwrap();
            let occurrences = module
                .ast
                .type_info
                .references
                .value_references
                .iter()
                .flat_map(|((module_name, symbol_name), references)| {
                    references
                        .iter()
                        .map(|reference| {
                            let (package_name, package_version) = if module_name == "gleam" {
                                (
                                    EcoString::from("gleam"),
                                    Version::parse(env!("CARGO_PKG_VERSION")).unwrap(),
                                )
                            } else {
                                let module_source = compiler.sources.get(module_name).unwrap();
                                (
                                    module_source.package_name.clone(),
                                    module_source.package_version.clone(),
                                )
                            };

                            let symbol = format_symbol(create_scip_symbol(
                                package_name.to_string(),
                                package_version.to_string(),
                                module_name.to_string(),
                                symbol_name.clone(),
                                // TODO: get proper kind
                                Suffix::Method,
                            ));

                            // Get start and end line
                            let start = this_module_source
                                .line_numbers
                                .line_and_column_number(reference.location.start);
                            let end = this_module_source
                                .line_numbers
                                .line_and_column_number(reference.location.end);

                            let byte_range = vec![
                                (start.line as i32) - 1,
                                (start.column as i32) - 1,
                                (end.line as i32) - 1,
                                (end.column as i32) - 1,
                            ];

                            let symbol_roles =
                                if symbols.iter().find(|sym| sym.symbol == symbol).is_some() {
                                    SymbolRole::Definition
                                } else {
                                    SymbolRole::UnspecifiedSymbolRole
                                };

                            Occurrence {
                                symbol,
                                range: byte_range,
                                symbol_roles: symbol_roles as i32,
                                ..Default::default()
                            }
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();

            Document {
                language: "gleam".to_string(),
                position_encoding: PositionEncoding::UTF8CodeUnitOffsetFromLineStart.into(),
                relative_path: module.input_path.to_string().replacen(
                    format!("{}/", path).as_str(),
                    "",
                    1,
                ),
                symbols,
                occurrences,
                ..Default::default()
            }
        })
        .collect::<Vec<_>>();

    let index = Index {
        metadata: Some(scip_metadata).into(),
        documents,
        ..Default::default()
    };

    write_message_to_file(output_file.clone(), index).map_err(|e| Error::FileIo {
        kind: FileKind::File,
        action: FileIoAction::WriteTo,
        path: output_file.clone().into(),
        err: Some(e.to_string()),
    })?;

    eprintln!("Wrote SCIP index to {}", output_file);

    Ok(())
}

fn create_scip_symbol(
    package_name: String,
    package_version: String,
    module_name: String,
    symbol_name: EcoString,
    suffix: Suffix,
) -> Symbol {
    return Symbol {
        scheme: "scip-gleam".into(),
        package: Some(Package {
            manager: "gleam".into(),
            name: package_name,
            version: package_version,
            ..Default::default()
        })
        .into(),
        descriptors: vec![
            Descriptor {
                name: module_name,
                suffix: Suffix::Package.into(),
                ..Default::default()
            },
            Descriptor {
                name: symbol_name.to_string(),
                suffix: suffix.into(),
                ..Default::default()
            },
        ],
        ..Default::default()
    };
}

fn format_doc(doc: Option<EcoString>) -> Vec<String> {
    doc.map(|doc| vec![doc.trim().to_string()])
        .unwrap_or(Vec::new())
}
