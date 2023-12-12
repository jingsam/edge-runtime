use crate::utils::units::mib_to_bytes;

use anyhow::{anyhow, bail, Error};
use deno_core::error::AnyError;
use deno_core::url::Url;
use deno_core::{located_script_name, serde_v8, JsRuntime, ModuleCode, ModuleId, RuntimeOptions};
use deno_http::DefaultHttpPropertyExtractor;
use deno_tls::rustls;
use deno_tls::rustls::RootCertStore;
use deno_tls::rustls_native_certs::load_native_certs;
use deno_tls::RootCertStoreProvider;
use log::error;
use serde::de::DeserializeOwned;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::snapshot;
use event_worker::events::{EventMetadata, WorkerEventWithMetadata};
use event_worker::js_interceptors::sb_events_js_interceptors;
use event_worker::sb_user_event_worker;
use sb_core::cache::CacheSetting;
use sb_core::cert::ValueRootCertStoreProvider;
use sb_core::external_memory::custom_allocator;
use sb_core::http_start::sb_core_http;
use sb_core::net::sb_core_net;
use sb_core::permissions::{sb_core_permissions, Permissions};
use sb_core::runtime::sb_core_runtime;
use sb_core::sb_core_main_js;
use sb_env::sb_env as sb_env_op;
use sb_graph::emitter::EmitterFactory;
use sb_graph::import_map::load_import_map;
use sb_graph::{generate_binary_eszip, EszipPayloadKind};
use sb_module_loader::standalone::create_module_loader_for_standalone_from_eszip_kind;
use sb_module_loader::RuntimeProviders;
use sb_node::deno_node;
use sb_workers::context::{UserWorkerMsgs, WorkerContextInitOpts, WorkerRuntimeOpts};
use sb_workers::sb_user_workers;

pub struct DenoRuntimeError(Error);

impl PartialEq for DenoRuntimeError {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_string() == other.0.to_string()
    }
}

impl fmt::Debug for DenoRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[Js Error] {}", self.0)
    }
}

fn get_error_class_name(e: &AnyError) -> &'static str {
    sb_core::errors_rt::get_error_class_name(e).unwrap_or("Error")
}

pub struct DenoRuntime {
    pub js_runtime: JsRuntime,
    pub env_vars: HashMap<String, String>, // TODO: does this need to be pub?
    main_module_id: ModuleId,
    pub conf: WorkerRuntimeOpts,
}

impl DenoRuntime {
    #[allow(clippy::unnecessary_literal_unwrap)]
    #[allow(clippy::arc_with_non_send_sync)]
    pub async fn new(opts: WorkerContextInitOpts) -> Result<Self, Error> {
        let WorkerContextInitOpts {
            service_path,
            no_module_cache,
            import_map_path,
            env_vars,
            events_rx,
            conf,
            maybe_eszip,
            maybe_entrypoint,
            maybe_module_code,
            ..
        } = opts;

        let user_agent = "supabase-edge-runtime".to_string();
        let base_dir_path = std::env::current_dir().map(|p| p.join(&service_path))?;
        let base_url = Url::from_directory_path(&base_dir_path).unwrap();

        // TODO: check for other potential main paths (eg: index.js, index.tsx)
        let mut main_module_url = base_url.join("index.ts")?;
        let is_some_entry_point = maybe_entrypoint.is_some();
        if is_some_entry_point {
            main_module_url = Url::parse(&maybe_entrypoint.unwrap())?;
        }

        let mut net_access_disabled = false;
        let mut allow_remote_modules = true;
        if conf.is_user_worker() {
            let user_conf = conf.as_user_worker().unwrap();
            net_access_disabled = user_conf.net_access_disabled;
            allow_remote_modules = user_conf.allow_remote_modules;
        }

        let mut maybe_arc_import_map = None;
        let only_module_code =
            maybe_module_code.is_some() && maybe_eszip.is_none() && !is_some_entry_point;

        let eszip = if let Some(eszip_payload) = maybe_eszip {
            eszip_payload
        } else {
            let mut emitter_factory = EmitterFactory::new();

            let cache_strategy = if no_module_cache {
                CacheSetting::ReloadAll
            } else {
                CacheSetting::Use
            };

            emitter_factory.set_file_fetcher_allow_remote(allow_remote_modules);
            emitter_factory.set_file_fetcher_cache_strategy(cache_strategy);

            let maybe_import_map = load_import_map(import_map_path.clone())?;
            emitter_factory.set_import_map(maybe_import_map);
            maybe_arc_import_map = emitter_factory.maybe_import_map.clone();

            let arc_emitter_factory = Arc::new(emitter_factory);

            let main_module_url_file_path = main_module_url.clone().to_file_path().unwrap();

            let maybe_code = if only_module_code {
                maybe_module_code
            } else {
                None
            };

            let eszip = generate_binary_eszip(
                main_module_url_file_path,
                arc_emitter_factory,
                maybe_code,
                import_map_path.clone(),
            )
            .await?;

            EszipPayloadKind::Eszip(eszip)
        };

        // Create and populate a root cert store based on environment variable.
        // Reference: https://github.com/denoland/deno/blob/v1.37.0/cli/args/mod.rs#L467
        let mut root_cert_store = RootCertStore::empty();
        let ca_stores: Vec<String> = (|| {
            let env_ca_store = std::env::var("DENO_TLS_CA_STORE").ok()?;
            Some(
                env_ca_store
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        })()
        .unwrap_or_else(|| vec!["mozilla".to_string()]);
        for store in ca_stores.iter() {
            match store.as_str() {
                "mozilla" => {
                    root_cert_store = deno_tls::create_default_root_cert_store();
                }
                "system" => {
                    let roots = load_native_certs().expect("could not load platform certs");
                    for root in roots {
                        root_cert_store
                            .add(&rustls::Certificate(root.0))
                            .expect("Failed to add platform cert to root cert store");
                    }
                }
                _ => {
                    bail!(
                        "Unknown certificate store \"{0}\" specified (allowed: \"system,mozilla\")",
                        store
                    );
                }
            }
        }

        let root_cert_store_provider: Arc<dyn RootCertStoreProvider> =
            Arc::new(ValueRootCertStoreProvider::new(root_cert_store.clone()));

        let mut stdio = Some(Default::default());
        if conf.is_user_worker() {
            stdio = Some(deno_io::Stdio {
                stdin: deno_io::StdioPipe::File(std::fs::File::create("/dev/null")?),
                stdout: deno_io::StdioPipe::File(std::fs::File::create("/dev/null")?),
                stderr: deno_io::StdioPipe::File(std::fs::File::create("/dev/null")?),
            });
        }

        let fs = Arc::new(deno_fs::RealFs);

        let rt_provider = create_module_loader_for_standalone_from_eszip_kind(
            eszip,
            maybe_arc_import_map,
            import_map_path,
        )
        .await?;

        let RuntimeProviders {
            npm_resolver,
            fs: file_system,
            module_loader,
            module_code,
        } = rt_provider;

        let mod_code = module_code;

        let extensions = vec![
            sb_core_permissions::init_ops(net_access_disabled),
            deno_webidl::deno_webidl::init_ops(),
            deno_console::deno_console::init_ops(),
            deno_url::deno_url::init_ops(),
            deno_web::deno_web::init_ops::<Permissions>(
                Arc::new(deno_web::BlobStore::default()),
                None,
            ),
            deno_fetch::deno_fetch::init_ops::<Permissions>(deno_fetch::Options {
                user_agent: user_agent.clone(),
                root_cert_store_provider: Some(root_cert_store_provider.clone()),
                ..Default::default()
            }),
            deno_websocket::deno_websocket::init_ops::<Permissions>(
                user_agent,
                Some(root_cert_store_provider.clone()),
                None,
            ),
            // TODO: support providing a custom seed for crypto
            deno_crypto::deno_crypto::init_ops(None),
            deno_broadcast_channel::deno_broadcast_channel::init_ops(
                deno_broadcast_channel::InMemoryBroadcastChannel::default(),
            ),
            deno_net::deno_net::init_ops::<Permissions>(Some(root_cert_store_provider), None),
            deno_tls::deno_tls::init_ops(),
            deno_http::deno_http::init_ops::<DefaultHttpPropertyExtractor>(),
            deno_io::deno_io::init_ops(stdio),
            deno_fs::deno_fs::init_ops::<Permissions>(fs.clone()),
            sb_env_op::init_ops(),
            sb_os::sb_os::init_ops(),
            sb_user_workers::init_ops(),
            sb_user_event_worker::init_ops(),
            sb_events_js_interceptors::init_ops(),
            sb_core_main_js::init_ops(),
            sb_core_net::init_ops(),
            sb_core_http::init_ops(),
            deno_node::init_ops::<Permissions>(Some(npm_resolver), file_system),
            sb_core_runtime::init_ops(Some(main_module_url.clone())),
        ];

        let mut create_params = None;
        if conf.is_user_worker() {
            let memory_limit =
                mib_to_bytes(conf.as_user_worker().unwrap().memory_limit_mb) as usize;
            create_params = Some(
                deno_core::v8::CreateParams::default()
                    .heap_limits(mib_to_bytes(0) as usize, memory_limit)
                    .array_buffer_allocator(custom_allocator(memory_limit)),
            )
        };
        let runtime_options = RuntimeOptions {
            extensions,
            is_main: true,
            create_params,
            get_error_class_fn: Some(&get_error_class_name),
            shared_array_buffer_store: None,
            compiled_wasm_module_store: Default::default(),
            startup_snapshot: Some(snapshot::snapshot()),
            module_loader: Some(module_loader),
            ..Default::default()
        };

        let mut js_runtime = JsRuntime::new(runtime_options);

        let version: Option<&str> = option_env!("GIT_V_TAG");

        // Bootstrapping stage
        let script = format!(
            "globalThis.bootstrapSBEdge({}, {}, {}, '{}')",
            deno_core::serde_json::json!({ "target": env!("TARGET") }),
            conf.is_user_worker(),
            conf.is_events_worker(),
            version.unwrap_or("0.1.0")
        );

        js_runtime
            .execute_script(located_script_name!(), ModuleCode::from(script))
            .expect("Failed to execute bootstrap script");

        {
            //run inside a closure, so op_state_rc is released
            let op_state_rc = js_runtime.op_state();
            let mut op_state = op_state_rc.borrow_mut();

            let mut env_vars = env_vars.clone();

            if conf.is_events_worker() {
                // if worker is an events worker, assert events_rx is to be available
                op_state
                    .put::<mpsc::UnboundedReceiver<WorkerEventWithMetadata>>(events_rx.unwrap());
            }

            if conf.is_user_worker() {
                let conf = conf.as_user_worker().unwrap();

                // set execution id for user workers
                env_vars.insert(
                    "SB_EXECUTION_ID".to_string(),
                    conf.key.map_or("".to_string(), |k| k.to_string()),
                );

                if let Some(events_msg_tx) = conf.events_msg_tx.clone() {
                    op_state.put::<mpsc::UnboundedSender<WorkerEventWithMetadata>>(events_msg_tx);
                    op_state.put::<EventMetadata>(EventMetadata {
                        service_path: conf.service_path.clone(),
                        execution_id: conf.key,
                    });
                }
            }

            op_state.put::<sb_env::EnvVars>(env_vars);
        }

        let main_module_id = js_runtime
            .load_main_module(&main_module_url, mod_code)
            .await?;

        Ok(Self {
            js_runtime,
            main_module_id,
            env_vars,
            conf,
        })
    }

    pub async fn run(
        mut self,
        unix_stream_rx: mpsc::UnboundedReceiver<UnixStream>,
    ) -> Result<(), Error> {
        {
            let op_state_rc = self.js_runtime.op_state();
            let mut op_state = op_state_rc.borrow_mut();
            op_state.put::<mpsc::UnboundedReceiver<UnixStream>>(unix_stream_rx);

            if self.conf.is_main_worker() {
                op_state.put::<mpsc::UnboundedSender<UserWorkerMsgs>>(
                    self.conf.as_main_worker().unwrap().worker_pool_tx.clone(),
                );
            }
        }

        let mut js_runtime = self.js_runtime;

        let future = async move {
            let mod_result_rx = js_runtime.mod_evaluate(self.main_module_id);
            match js_runtime.run_event_loop(false).await {
                Err(err) => {
                    // usually this happens because isolate is terminated
                    error!("event loop error: {}", err);
                    Err(anyhow!("event loop error: {}", err))
                }
                Ok(_) => match mod_result_rx.await {
                    Err(_) => Err(anyhow!("mod result sender dropped")),
                    Ok(Err(err)) => {
                        error!("{}", err.to_string());
                        Err(err)
                    }
                    Ok(Ok(_)) => Ok(()),
                },
            }
        };

        // need to set an explicit timeout here in case the event loop idle
        let mut duration = Duration::MAX;
        if self.conf.is_user_worker() {
            let worker_timeout_ms = self.conf.as_user_worker().unwrap().worker_timeout_ms;
            duration = Duration::from_millis(worker_timeout_ms);
        }
        match tokio::time::timeout(duration, future).await {
            Err(_) => Err(anyhow!("wall clock duration reached")),
            Ok(res) => res,
        }
    }

    #[allow(clippy::wrong_self_convention)]
    // TODO: figure out why rustc complains about this
    #[allow(dead_code)]
    fn to_value<T>(
        &mut self,
        global_value: &deno_core::v8::Global<deno_core::v8::Value>,
    ) -> Result<T, AnyError>
    where
        T: DeserializeOwned + 'static,
    {
        let scope = &mut self.js_runtime.handle_scope();
        let value = deno_core::v8::Local::new(scope, global_value.clone());
        Ok(serde_v8::from_v8(scope, value)?)
    }
}

#[cfg(test)]
mod test {
    use crate::deno_runtime::DenoRuntime;
    use deno_core::{FastString, ModuleCode};
    use sb_graph::emitter::EmitterFactory;
    use sb_graph::{generate_binary_eszip, EszipPayloadKind};
    use sb_workers::context::{
        MainWorkerRuntimeOpts, UserWorkerMsgs, UserWorkerRuntimeOpts, WorkerContextInitOpts,
        WorkerRuntimeOpts,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::fs::File;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::net::UnixStream;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_module_code_no_eszip() {
        let (worker_pool_tx, _) = mpsc::unbounded_channel::<UserWorkerMsgs>();
        DenoRuntime::new(WorkerContextInitOpts {
            service_path: PathBuf::from("./test_cases/"),
            no_module_cache: false,
            import_map_path: None,
            env_vars: Default::default(),
            events_rx: None,
            timing_rx_pair: None,
            maybe_eszip: None,
            maybe_entrypoint: None,
            maybe_module_code: Some(FastString::from(String::from(
                "Deno.serve((req) => new Response('Hello World'));",
            ))),
            conf: { WorkerRuntimeOpts::MainWorker(MainWorkerRuntimeOpts { worker_pool_tx }) },
        })
        .await
        .expect("It should not panic");
    }

    #[tokio::test]
    #[allow(clippy::arc_with_non_send_sync)]
    async fn test_eszip_with_source_file() {
        let (worker_pool_tx, _) = mpsc::unbounded_channel::<UserWorkerMsgs>();
        let mut file = File::create("./test_cases/eszip-source-test.ts").unwrap();
        file.write_all(b"import isEven from \"npm:is-even\"; globalThis.isTenEven = isEven(9);")
            .unwrap();
        let path_buf = PathBuf::from("./test_cases/eszip-source-test.ts");
        let emitter_factory = Arc::new(EmitterFactory::new());
        let bin_eszip = generate_binary_eszip(path_buf, emitter_factory.clone(), None, None)
            .await
            .unwrap();
        fs::remove_file("./test_cases/eszip-source-test.ts").unwrap();

        let eszip_code = bin_eszip.into_bytes();

        let runtime = DenoRuntime::new(WorkerContextInitOpts {
            service_path: PathBuf::from("./test_cases/"),
            no_module_cache: false,
            import_map_path: None,
            env_vars: Default::default(),
            events_rx: None,
            timing_rx_pair: None,
            maybe_eszip: Some(EszipPayloadKind::VecKind(eszip_code)),
            maybe_entrypoint: None,
            maybe_module_code: None,
            conf: { WorkerRuntimeOpts::MainWorker(MainWorkerRuntimeOpts { worker_pool_tx }) },
        })
        .await;

        let mut rt = runtime.unwrap();

        let main_mod_ev = rt.js_runtime.mod_evaluate(rt.main_module_id);
        let _ = rt.js_runtime.run_event_loop(false).await;

        let read_is_even_global = rt
            .js_runtime
            .execute_script(
                "<anon>",
                ModuleCode::from(
                    r#"
            globalThis.isTenEven;
        "#
                    .to_string(),
                ),
            )
            .unwrap();
        let read_is_even = rt.to_value::<deno_core::serde_json::Value>(&read_is_even_global);
        assert_eq!(read_is_even.unwrap().to_string(), "false");
        std::mem::drop(main_mod_ev);
    }

    #[tokio::test]
    #[allow(clippy::arc_with_non_send_sync)]
    async fn test_create_eszip_from_graph() {
        let (worker_pool_tx, _) = mpsc::unbounded_channel::<UserWorkerMsgs>();
        let file = PathBuf::from("./test_cases/eszip-silly-test/index.ts");
        let service_path = PathBuf::from("./test_cases/eszip-silly-test");
        let emitter_factory = Arc::new(EmitterFactory::new());
        let binary_eszip = generate_binary_eszip(file, emitter_factory.clone(), None, None)
            .await
            .unwrap();

        let eszip_code = binary_eszip.into_bytes();

        let runtime = DenoRuntime::new(WorkerContextInitOpts {
            service_path,
            no_module_cache: false,
            import_map_path: None,
            env_vars: Default::default(),
            events_rx: None,
            timing_rx_pair: None,
            maybe_eszip: Some(EszipPayloadKind::VecKind(eszip_code)),
            maybe_entrypoint: None,
            maybe_module_code: None,
            conf: { WorkerRuntimeOpts::MainWorker(MainWorkerRuntimeOpts { worker_pool_tx }) },
        })
        .await;

        let mut rt = runtime.unwrap();

        let main_mod_ev = rt.js_runtime.mod_evaluate(rt.main_module_id);
        let _ = rt.js_runtime.run_event_loop(false).await;

        let read_is_even_global = rt
            .js_runtime
            .execute_script(
                "<anon>",
                ModuleCode::from(
                    r#"
            globalThis.isTenEven;
        "#
                    .to_string(),
                ),
            )
            .unwrap();
        let read_is_even = rt.to_value::<deno_core::serde_json::Value>(&read_is_even_global);
        assert_eq!(read_is_even.unwrap().to_string(), "true");
        std::mem::drop(main_mod_ev);
    }

    async fn create_runtime(
        path: Option<PathBuf>,
        env_vars: Option<HashMap<String, String>>,
        user_conf: Option<WorkerRuntimeOpts>,
    ) -> DenoRuntime {
        let (worker_pool_tx, _) = mpsc::unbounded_channel::<UserWorkerMsgs>();

        DenoRuntime::new(WorkerContextInitOpts {
            service_path: path.unwrap_or(PathBuf::from("./test_cases/main")),
            no_module_cache: false,
            import_map_path: None,
            env_vars: env_vars.unwrap_or_default(),
            events_rx: None,
            timing_rx_pair: None,
            maybe_eszip: None,
            maybe_entrypoint: None,
            maybe_module_code: None,
            conf: {
                if let Some(uc) = user_conf {
                    uc
                } else {
                    WorkerRuntimeOpts::MainWorker(MainWorkerRuntimeOpts { worker_pool_tx })
                }
            },
        })
        .await
        .unwrap()
    }

    // Main Runtime should have access to `EdgeRuntime`
    #[tokio::test]
    async fn test_main_runtime_creation() {
        let mut runtime = create_runtime(None, None, None).await;

        {
            let scope = &mut runtime.js_runtime.handle_scope();
            let context = scope.get_current_context();
            let inner_scope = &mut deno_core::v8::ContextScope::new(scope, context);
            let global = context.global(inner_scope);
            let edge_runtime_key: deno_core::v8::Local<deno_core::v8::Value> =
                deno_core::serde_v8::to_v8(inner_scope, "EdgeRuntime").unwrap();
            assert!(!global
                .get(inner_scope, edge_runtime_key)
                .unwrap()
                .is_undefined(),);
        }
    }

    // User Runtime Should not have access to EdgeRuntime
    #[tokio::test]
    async fn test_user_runtime_creation() {
        let mut runtime = create_runtime(
            None,
            None,
            Some(WorkerRuntimeOpts::UserWorker(Default::default())),
        )
        .await;

        {
            let scope = &mut runtime.js_runtime.handle_scope();
            let context = scope.get_current_context();
            let inner_scope = &mut deno_core::v8::ContextScope::new(scope, context);
            let global = context.global(inner_scope);
            let edge_runtime_key: deno_core::v8::Local<deno_core::v8::Value> =
                deno_core::serde_v8::to_v8(inner_scope, "EdgeRuntime").unwrap();
            assert!(global
                .get(inner_scope, edge_runtime_key)
                .unwrap()
                .is_undefined(),);
        }
    }

    #[tokio::test]
    async fn test_main_rt_fs() {
        let mut main_rt = create_runtime(None, Some(std::env::vars().collect()), None).await;

        let global_value_deno_read_file_script = main_rt
            .js_runtime
            .execute_script(
                "<anon>",
                ModuleCode::from(
                    r#"
            Deno.readTextFileSync("./test_cases/readFile/hello_world.json");
        "#
                    .to_string(),
                ),
            )
            .unwrap();
        let fs_read_result =
            main_rt.to_value::<deno_core::serde_json::Value>(&global_value_deno_read_file_script);
        assert_eq!(
            fs_read_result.unwrap().as_str().unwrap(),
            "{\n  \"hello\": \"world\"\n}"
        );
    }

    // #[tokio::test]
    // async fn test_node_builtin_imports() {
    //     let mut main_rt = create_runtime(
    //         Some(PathBuf::from("./test_cases/node-built-in")),
    //         Some(std::env::vars().collect()),
    //         None,
    //     )
    //     .await;
    //     let mod_evaluate = main_rt.js_runtime.mod_evaluate(main_rt.main_module_id);
    //     let _ = main_rt.js_runtime.run_event_loop(false).await;
    //     let global_value_deno_read_file_script = main_rt
    //         .js_runtime
    //         .execute_script(
    //             "<anon>",
    //             r#"
    //         globalThis.basename('/Users/Refsnes/demo_path.js');
    //     "#,
    //         )
    //         .unwrap();
    //     let fs_read_result =
    //         main_rt.to_value::<deno_core::serde_json::Value>(&global_value_deno_read_file_script);
    //     assert_eq!(fs_read_result.unwrap().as_str().unwrap(), "demo_path.js");
    //     std::mem::drop(mod_evaluate);
    // }

    #[tokio::test]
    async fn test_os_ops() {
        let mut user_rt = create_runtime(
            None,
            None,
            Some(WorkerRuntimeOpts::UserWorker(Default::default())),
        )
        .await;

        let user_rt_execute_scripts = user_rt
            .js_runtime
            .execute_script(
                "<anon>",
                ModuleCode::from(
                    r#"
            // Should not be able to set
            const data = {
                gid: Deno.gid(),
                uid: Deno.uid(),
                hostname: Deno.hostname(),
                loadavg: Deno.loadavg(),
                osUptime: Deno.osUptime(),
                osRelease: Deno.osRelease(),
                systemMemoryInfo: Deno.systemMemoryInfo(),
                consoleSize: Deno.consoleSize(),
                version: [Deno.version.deno, Deno.version.v8, Deno.version.typescript],
                networkInterfaces: Deno.networkInterfaces()
            };
            data;
        "#
                    .to_string(),
                ),
            )
            .unwrap();
        let serde_deno_env = user_rt
            .to_value::<deno_core::serde_json::Value>(&user_rt_execute_scripts)
            .unwrap();
        assert_eq!(serde_deno_env.get("gid").unwrap().as_i64().unwrap(), 1000);
        assert_eq!(serde_deno_env.get("uid").unwrap().as_i64().unwrap(), 1000);
        assert!(serde_deno_env.get("osUptime").unwrap().as_i64().unwrap() > 0);
        assert_eq!(
            serde_deno_env.get("osRelease").unwrap().as_str().unwrap(),
            "0.0.0-00000000-generic"
        );

        let loadavg_array = serde_deno_env
            .get("loadavg")
            .unwrap()
            .as_array()
            .unwrap()
            .to_vec();
        assert_eq!(loadavg_array.first().unwrap().as_f64().unwrap(), 0.0);
        assert_eq!(loadavg_array.get(1).unwrap().as_f64().unwrap(), 0.0);
        assert_eq!(loadavg_array.get(2).unwrap().as_f64().unwrap(), 0.0);

        let network_interfaces_data = serde_deno_env
            .get("networkInterfaces")
            .unwrap()
            .as_array()
            .unwrap()
            .to_vec();
        assert_eq!(network_interfaces_data.len(), 2);

        let deno_version_array = serde_deno_env
            .get("version")
            .unwrap()
            .as_array()
            .unwrap()
            .to_vec();
        assert_eq!(
            deno_version_array.first().unwrap().as_str().unwrap(),
            "supabase-edge-runtime-0.1.0"
        );
        assert_eq!(
            deno_version_array.get(1).unwrap().as_str().unwrap(),
            "11.6.189.12"
        );
        assert_eq!(
            deno_version_array.get(2).unwrap().as_str().unwrap(),
            "5.1.6"
        );

        let system_memory_info_map = serde_deno_env
            .get("systemMemoryInfo")
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        assert!(system_memory_info_map.contains_key("total"));
        assert!(system_memory_info_map.contains_key("free"));
        assert!(system_memory_info_map.contains_key("available"));
        assert!(system_memory_info_map.contains_key("buffers"));
        assert!(system_memory_info_map.contains_key("cached"));
        assert!(system_memory_info_map.contains_key("swapTotal"));
        assert!(system_memory_info_map.contains_key("swapFree"));

        let deno_consle_size_map = serde_deno_env
            .get("consoleSize")
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        assert!(deno_consle_size_map.contains_key("rows"));
        assert!(deno_consle_size_map.contains_key("columns"));

        let user_rt_execute_scripts = user_rt.js_runtime.execute_script(
            "<anon>",
            ModuleCode::from(
                r#"
            let cmd = new Deno.Command("", {});
            cmd.outputSync();
        "#
                .to_string(),
            ),
        );
        assert!(user_rt_execute_scripts.is_err());
        assert!(user_rt_execute_scripts
            .unwrap_err()
            .to_string()
            .contains("Spawning subprocesses is not allowed on Supabase Edge Runtime"));
    }

    #[tokio::test]
    async fn test_os_env_vars() {
        std::env::set_var("Supa_Test", "Supa_Value");
        let mut main_rt = create_runtime(None, Some(std::env::vars().collect()), None).await;
        let mut user_rt = create_runtime(
            None,
            None,
            Some(WorkerRuntimeOpts::UserWorker(Default::default())),
        )
        .await;
        assert!(!main_rt.env_vars.is_empty());
        assert!(user_rt.env_vars.is_empty());

        let err = main_rt
            .js_runtime
            .execute_script(
                "<anon>",
                ModuleCode::from(
                    r#"
            // Should not be able to set
            Deno.env.set("Supa_Test", "Supa_Value");
        "#
                    .to_string(),
                ),
            )
            .err()
            .unwrap();
        assert!(err
            .to_string()
            .contains("NotSupported: The operation is not supported"));

        let main_deno_env_get_supa_test = main_rt
            .js_runtime
            .execute_script(
                "<anon>",
                ModuleCode::from(
                    r#"
            // Should not be able to set
            Deno.env.get("Supa_Test");
        "#
                    .to_string(),
                ),
            )
            .unwrap();
        let serde_deno_env =
            main_rt.to_value::<deno_core::serde_json::Value>(&main_deno_env_get_supa_test);
        assert_eq!(serde_deno_env.unwrap().as_str().unwrap(), "Supa_Value");

        // User does not have this env variable because it was not provided
        // During the runtime creation
        let user_deno_env_get_supa_test = user_rt
            .js_runtime
            .execute_script(
                "<anon>",
                ModuleCode::from(
                    r#"
            // Should not be able to set
            Deno.env.get("Supa_Test");
        "#
                    .to_string(),
                ),
            )
            .unwrap();
        let user_serde_deno_env =
            user_rt.to_value::<deno_core::serde_json::Value>(&user_deno_env_get_supa_test);
        assert!(user_serde_deno_env.unwrap().is_null());
    }

    async fn create_basic_user_runtime(
        path: &str,
        memory_limit: u64,
        worker_timeout_ms: u64,
    ) -> DenoRuntime {
        create_runtime(
            Some(PathBuf::from(path)),
            None,
            Some(WorkerRuntimeOpts::UserWorker(UserWorkerRuntimeOpts {
                memory_limit_mb: memory_limit,
                worker_timeout_ms,
                cpu_time_soft_limit_ms: 100,
                cpu_time_hard_limit_ms: 200,
                low_memory_multiplier: 5,
                force_create: true,
                net_access_disabled: false,
                allow_remote_modules: true,
                custom_module_root: None,
                key: None,
                pool_msg_tx: None,
                events_msg_tx: None,
                service_path: None,
            })),
        )
        .await
    }

    #[tokio::test]
    async fn test_read_file_user_rt() {
        let user_rt = create_basic_user_runtime("./test_cases/readFile", 20, 1000).await;
        let (_tx, unix_stream_rx) = mpsc::unbounded_channel::<UnixStream>();
        let result = user_rt.run(unix_stream_rx).await;
        match result {
            Err(err) => {
                assert!(err
                    .to_string()
                    .contains("TypeError: Deno.readFileSync is not a function"));
            }
            _ => panic!("Invalid Result"),
        };
    }

    #[tokio::test]
    async fn test_array_buffer_allocation_below_limit() {
        let user_rt = create_basic_user_runtime("./test_cases/array_buffers", 20, 1000).await;
        let (_tx, unix_stream_rx) = mpsc::unbounded_channel::<UnixStream>();
        let result = user_rt.run(unix_stream_rx).await;
        assert!(result.is_ok(), "expected no errors");
    }

    #[tokio::test]
    async fn test_array_buffer_allocation_above_limit() {
        let user_rt = create_basic_user_runtime("./test_cases/array_buffers", 15, 1000).await;
        let (_tx, unix_stream_rx) = mpsc::unbounded_channel::<UnixStream>();
        let result = user_rt.run(unix_stream_rx).await;
        match result {
            Err(err) => {
                assert!(err
                    .to_string()
                    .contains("RangeError: Array buffer allocation failed"));
            }
            _ => panic!("Invalid Result"),
        };
    }
}
