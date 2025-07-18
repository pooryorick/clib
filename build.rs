use anyhow::{
    Context,
    Result,
    anyhow,
};

use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    fmt::Debug,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

type Toml = toml::value::Value;

const UTF8_PATH: &'static str = "path should be valid UTF-8 string.";
const PKG_NAME_IS_STR: &'static str = "pkg name should be str.";

fn check_os( table: &toml::Table ) -> Result<bool> {
    if let Some( os ) = table .get("os") {
        let os = os.as_str().context( "os name should be str." )?;
        Ok( match_os( os ))
    } else {
        Ok( true )
    }
}

fn match_os( name: &str ) -> bool {
    match name {
        "android"   => if cfg!( target_os = "android"   ) {true} else {false},
        "dragonfly" => if cfg!( target_os = "dragonfly" ) {true} else {false},
        "freebsd"   => if cfg!( target_os = "freebsd"   ) {true} else {false},
        "ios"       => if cfg!( target_os = "ios"       ) {true} else {false},
        "linux"     => if cfg!( target_os = "linux"     ) {true} else {false},
        "macos"     => if cfg!( target_os = "macos"     ) {true} else {false},
        "netbsd"    => if cfg!( target_os = "netbsd"    ) {true} else {false},
        "openbsd"   => if cfg!( target_os = "openbsd"   ) {true} else {false},
        "windows"   => if cfg!( target_os = "windows"   ) {true} else {false},
        "unix"      => if cfg!(              unix       ) {true} else {false},
        _           => false,
    }
}

#[derive( Debug )]
pub struct LibInfo {
    link_paths    : RefCell<Vec<String>>,
    include_paths : RefCell<Vec<String>>,
    headers       : RefCell<Vec<String>>,
    specs         : HashMap<String,Toml>,
}

impl LibInfo {
    fn new( specs: HashMap<String,Toml> ) -> Self {
        LibInfo {
            link_paths    : RefCell::default(),
            include_paths : RefCell::default(),
            headers       : RefCell::default(),
            specs         ,
        }
    }

    fn probe( &self, pkg_name: &str, scan_incdir: bool ) -> Result<()> {
        let probed_ex = self
            .probe_via_pkgconf( pkg_name, scan_incdir )
            .or_else( |_| self.probe_via_search( pkg_name, scan_incdir ))?;

        if scan_incdir {
            self.include_paths.borrow_mut().push( self.get_includedir( &probed_ex )? );
        }

        if let Some( spec ) = self.specs.get( pkg_name ) {
            let include_dir = self.get_includedir( &probed_ex )?;

            if let Some( table ) = spec.as_table() {
                if !scan_incdir {
                    table
                        .get( "headers" )
                        .and_then( |headers| headers.as_array() )
                        .and_then( |headers| headers
                            .iter()
                            .try_for_each( |header| -> Result<()> {
                                if let Some( header ) = header.as_str() {
                                    self.headers.borrow_mut().push(
                                        Path::new( &include_dir )
                                            .join( header )
                                            .to_str()
                                            .context( UTF8_PATH )?
                                            .to_owned()
                                    )
                                }
                                Ok(())
                            })
                            .ok()
                        ).context( UTF8_PATH )?;

                    if !probed_ex.pkgconf_ok() {
                        if let Some( dependencies ) = table.get( "dependencies" ) {
                            match dependencies {
                                Toml::Array( dependencies ) => for pkg_name in dependencies {
                                    self.probe( pkg_name.as_str().context( PKG_NAME_IS_STR )?, false )?;
                                },
                                Toml::Table( dependencies ) => for (pkg_name, dep) in dependencies {
                                    let dep = dep.as_table().context("named dependency should be table.")?;
                                    if check_os( dep )? {
                                        self.probe( pkg_name, false )?;
                                    }
                                },
                                _ => return Err( anyhow!( "invalid dependencies." )),
                            }
                        }
                    }
                }

                if let Some( dependencies ) = table.get( "header-dependencies" ) {
                    match dependencies {
                        Toml::Array( dependencies ) => for pkg_name in dependencies {
                            self.probe( pkg_name.as_str().context( PKG_NAME_IS_STR )?, true )?;
                        },
                        Toml::Table( dependencies ) => for (pkg_name, dep) in dependencies {
                            let dep = dep.as_table().context("named dependency should be table.")?;
                            if check_os( dep )? {
                                self.probe( pkg_name, true )?;
                            }
                        },
                        _ => return Err( anyhow!( "invalid header-dependencies." )),
                    }
                }
            }
        }
        Ok(())
    }

    fn probe_via_pkgconf( &self, pkg_name: &str, scan_incdir: bool ) -> Result<ProbedEx> {
        env::set_var( "PKG_CONFIG_ALLOW_SYSTEM_CFLAGS", "1" );
        env::set_var( "PKG_CONFIG_ALLOW_SYSTEM_LIBS", "1" );

        let mut cfg = pkg_config::Config::new();
        cfg.cargo_metadata( true );

        let mut pc_file_names = vec![ pkg_name ];

        if let Some( spec ) = self.specs.get( pkg_name ) {
            let table = spec.as_table().expect("clib specs should be a table.");
            if let Some( pc_alias ) = table.get("pc-alias") {
                pc_alias
                    .as_array()
                    .expect("pc-alias should be array.")
                    .iter()
                    .for_each( |pc| {
                        pc_file_names.push( pc.as_str().expect( ".pc file name should be str." ));
                    });
            }
        }

        let mut names = pc_file_names.into_iter();
        let (library, pc_name) = loop {
            if let Some( name ) = names.next() {
                if let Ok( library ) = cfg.probe( name ) {
                    break (library, name.to_owned() );
                }
            } else {
                return Err( anyhow!( "failed to locate .pc file" ));
            }
        };

        if !scan_incdir {
            library.link_paths
                .into_iter()
                .map( |path| path.to_str().expect( UTF8_PATH ).to_owned() )
                .for_each( |link_path| self.link_paths.borrow_mut().push( link_path ));

            library.include_paths
                .into_iter()
                .map( |path| path.to_str().expect( UTF8_PATH ).to_owned() )
                .for_each( |include_path| self.include_paths.borrow_mut().push( include_path ));
        }

        Ok( ProbedEx::PcName( pc_name ))
    }

    fn probe_via_search( &self, pkg_name: &str, scan_incdir: bool ) -> Result<ProbedEx> {
        //if cfg!( unix ) {
        //    return Err( anyhow!( "failed in using pkg-config for probe library" ));
        //}

        if let Some( table ) = self.specs
            .get( pkg_name )
            .unwrap()
            .as_table()
        {
            if let Some( executable_names ) = table.get( "exe" ).and_then( |exe| exe.as_array() ) {
                for name in executable_names {
                    let name = name.as_str().expect("exe names should be str.");
                    let output = Command::new( if cfg!(unix) { "which" } else { "where" })
                        .arg( name ).output();
                    match output {
                        Ok( output ) => {
                            let s = output.stdout.as_slice();
                            if s.is_empty() {
                                continue;
                            }
                            let cmd_path = Path::new( std::str::from_utf8( s )
                                .expect( UTF8_PATH )
                                .trim_end() );

                            let parent = cmd_path.parent()
                                .expect("executable should not be found in root directory.");
                            assert_eq!( parent.file_name().expect( UTF8_PATH ), "bin" );
                            let prefix = parent.parent()
                                .expect("bin should not be found in root directory.");
                            let include_base = prefix.join("include");

                            let guess_include = table
                                .get("includedir")
                                .and_then( |includedirs| includedirs.as_array() )
                                .and_then( |dirs| Some( dirs.iter().map( |dir| dir.as_str().expect( "include dir should be str." ))))
                                .and_then( |dirs| {
                                    for dir in dirs {
                                        let dir = include_base.join( dir );
                                        if dir.exists() {
                                            return Some( dir.to_str().expect( UTF8_PATH ).to_owned() );
                                        }
                                    }
                                    Some( include_base.to_str().expect( UTF8_PATH ).to_owned() )
                                })
                                .expect("include_path");

                            if !scan_incdir {
                                self.link_paths.borrow_mut().push( prefix.join("lib").to_str().expect( UTF8_PATH ).to_owned() );
                                println!( "cargo:rustc-link-search=native={}/lib", prefix.to_str().expect( UTF8_PATH ));
                                emit_cargo_meta_for_libs( &prefix, table.get( "libs" ).expect( "metadata should contain libs" ))?;
                                if let Some( libs ) = table.get( "libs-private" ) {
                                    emit_cargo_meta_for_libs( &prefix, libs )?;
                                }
                            }
                            return Ok( ProbedEx::IncDir( guess_include ));
                        },
                        Err(_) => continue,
                    }
                }
                return Err( anyhow!( "executable not found" ));
            } else {
                return Err( anyhow!( "failed to locate executable" ));
            }
        } else {
            return Err( anyhow!( "failed to search lib." ));
        }
    }

    fn get_includedir( &self, probe_ex: &ProbedEx ) -> Result<String> {
        match probe_ex {
            ProbedEx::PcName( pc_name ) => {
                let exe = env::var( "PKG_CONFIG" ).unwrap_or_else( |_| "pkg-config".to_owned() );
                let mut cmd = Command::new( exe );
                cmd.args( &[ &pc_name, "--variable", "includedir" ]);

                let output = cmd.output()?;
                let result = Ok( std::str::from_utf8( output.stdout.as_slice() )?
                    .trim_end().to_owned() );
                result
            },
            ProbedEx::IncDir( includedir ) => {
                let path = Path::new( &includedir );
                assert!( path.exists() );
                Ok( format!( "{}", path.display() ))
            },
        }
    }
}

fn emit_cargo_meta_for_libs( prefix: &Path, value: &Toml ) -> Result<()> {
    let lib_path = prefix.join("lib");

    if let Some( table ) = value.as_table() {
        'values:
        for value in table.values() {
            let lib_names = value.as_array().expect("names of libs should be an array.");
            for lib_name in lib_names {
                let lib_name = lib_name.as_str().expect( "lib name should be str." );
                if lib_path.join( lib_name ).exists() {
                    println!( "cargo:rustc-link-lib={}", get_link_name( lib_name ));
                    continue 'values;
                }
            }
            return Err( anyhow!( "lib should be found in {:?} directory.", lib_path ));
        }
    } else if let Some( lib_names ) = value.as_array() {
        for lib_name in lib_names {
            let lib_name = lib_name.as_str().expect("lib name should be str.");
            if lib_path.join( lib_name ).exists() {
                println!( "cargo:rustc-link-lib={}", get_link_name( lib_name ));
            } else {
                return Err( anyhow!( "failed to locate {}", lib_name ));
            }
        }
    }
    Ok(())
}

fn get_link_name( lib_name: &str ) -> &str {
    let start = if lib_name.starts_with( "lib" ) { 3 } else { 0 };
    match lib_name.rfind('.') {
        Some( dot ) => &lib_name[ start..dot ],
        None => &lib_name[ start.. ],
    }
}

enum ProbedEx {
    IncDir( String ),
    PcName( String ),
}

impl ProbedEx {
    fn pkgconf_ok( &self ) -> bool {
        match self {
            ProbedEx::IncDir(_)  => false,
            ProbedEx::PcName(_)  => true,
        }
    }
}

fn generate_dummy() {
    let out_path = PathBuf::from( env::var( "OUT_DIR" ).expect( "$OUT_DIR should exist." ));
    File::create( out_path.join( "bindings.rs" )).expect( "an empty bindings.rs generated." );
}

fn main() {
    let (specs, builds) = inwelling::collect_downstream( inwelling::Opts::default() )
        .packages
        .into_iter()
        .fold(
            (
                HashMap::<String,Toml>::new(),    // pkg name -> spec
                HashMap::<String,PathBuf>::new(), // builds -> the path of downstream's manifest
            ),
            |(mut specs, mut builds), package| {
                package.metadata
                    .as_table()
                    .map( |table| {
                        table
                            .get( "spec" )
                            .and_then( |spec| spec.as_table() )
                            .map( |spec| spec.iter()
                                .for_each( |(key,value)| { specs.insert( key.clone(), value.clone() ); }));
                        table
                            .get( "build" )
                            .and_then( |build| build.as_array() )
                            .map( |build_list| build_list.iter()
                                .for_each( |pkg| { pkg.as_str().map( |pkg| { builds.insert( pkg.to_owned(), package.manifest.clone() ); }); }));
                    });
                (specs, builds)
            }
        );

    if builds.is_empty() {
        generate_dummy();
        return;
    }

    #[cfg( target_os = "freebsd" )]
    env::set_var( "PKG_CONFIG_ALLOW_CROSS", "1" );

    let lib_info_all = LibInfo::new( specs );

    let mut downstream_files_for_docs_rs = Vec::<PathBuf>::new();

    builds.iter().for_each( |(pkg_name, manifest_path)| {
        if !pkg_name.is_empty() {
            match lib_info_all.probe( pkg_name, false ) {
                Ok(_) => (),
                Err( err ) => {
                    //if cfg!( target_os = "linux" ) && Path::new( "/.dockerenv" ).exists() {
                        // make docs.rs happy
                        println!( "cargo:warning=[clib] fails to probe library {}, error occured: {:?}", pkg_name, err );
                        if let Some( spec ) = lib_info_all.specs.get( pkg_name ) {
                            if let Some( table ) = spec.as_table() {
                                if let Some( for_docs_rs ) = table.get( "for-docs-rs" ) {
                                    if let Some( for_docs_rs ) = for_docs_rs.as_str() {
                                        downstream_files_for_docs_rs.push(
                                            manifest_path
                                                .parent()
                                                .expect("the manifest dir")
                                                .join( for_docs_rs )
                                        );
                                    }
                                }
                            }
                        }
                    //} else {
                    //    panic!( "{:#?}", err );
                    //}
                },
            }
        }
    });

    let out_path = PathBuf::from( env::var( "OUT_DIR" ).expect( "$OUT_DIR should exist." ));

    if !lib_info_all.headers.borrow().is_empty() {

        let mut builder = bindgen_helpers::Builder::default()
            .generate_comments( false )
        ;

        for header in lib_info_all.headers.borrow().iter() {
            builder = builder.header( header );
        }
        for path in lib_info_all.include_paths.borrow().iter() {
            let opt = format!( "-I{}", path );
            builder = builder.clang_arg( &opt );
        }

        let bindings = builder.generate().expect( "bindgen builder constructed." );
        bindings.write_to_file( out_path.join( "bindings.rs" )).expect( "bindings.rs generated." );
    } else if downstream_files_for_docs_rs.is_empty() {
        generate_dummy();
    } else {
        let mut out_file = File::create( out_path.join( "bindings.rs" ) )
            .expect( &format!( "{:?} should be created for add contents for docs.rs.", out_path ));
        for path in &downstream_files_for_docs_rs {
            let contents = fs::read_to_string( path )
                .expect( &format!( "contents for generating docs on docs.rs should be read from {:?}", path ));
            writeln!( &mut out_file, "{}", contents )
                .expect( &format!( "Some contents for generating docs on docs.rs should be appended to {:?}.", out_path ));
        }
    }
}
