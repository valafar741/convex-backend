use std::{
    collections::{
        btree_map::Entry,
        BTreeMap,
    },
    path::Path,
    str::FromStr,
};

use anyhow::anyhow;
use common::{
    errors::JsError,
    knobs::{
        DATABASE_UDF_SYSTEM_TIMEOUT,
        DATABASE_UDF_USER_TIMEOUT,
    },
    log_lines::LogLine,
    runtime::{
        Runtime,
        UnixTimestamp,
    },
    types::{
        HttpActionRoute,
        RoutableMethod,
        UdfType,
    },
};
use deno_core::{
    v8::{
        self,
        GetPropertyNamesArgs,
        HandleScope,
    },
    ModuleResolutionError,
};
use errors::ErrorMetadata;
use model::{
    config::types::{
        ModuleConfig,
        ModuleEnvironment,
    },
    cron_jobs::types::{
        CronIdentifier,
        CronSpec,
    },
    environment_variables::types::{
        EnvVarName,
        EnvVarValue,
    },
    modules::{
        args_validator::ArgsValidator,
        module_versions::{
            AnalyzedFunction,
            AnalyzedHttpRoute,
            AnalyzedModule,
            AnalyzedSourcePosition,
            FunctionName,
            MappedModule,
            ModuleSource,
            SourceMap,
            Visibility,
        },
    },
    udf_config::types::UdfConfig,
};
use rand::SeedableRng;
use rand_chacha::ChaCha12Rng;
use serde_json::Value as JsonValue;
use sync_types::{
    CanonicalizedModulePath,
    ModulePath,
};
use value::{
    heap_size::WithHeapSize,
    TableMapping,
    TableMappingValue,
    VirtualTableMapping,
};

use crate::{
    concurrency_limiter::ConcurrencyPermit,
    environment::{
        helpers::{
            module_loader::{
                module_specifier_from_path,
                module_specifier_from_str,
                path_from_module_specifier,
            },
            syscall_error::{
                syscall_description_for_error,
                syscall_name_for_error,
            },
        },
        AsyncOpRequest,
        IsolateEnvironment,
    },
    execution_scope::ExecutionScope,
    helpers,
    isolate::Isolate,
    metrics::{
        log_source_map_missing,
        log_source_map_origin_in_separate_module,
        log_source_map_token_lookup_failed,
    },
    request_scope::RequestScope,
    strings,
    timeout::Timeout,
    user_error::ModuleNotFoundError,
};

pub struct AnalyzeEnvironment {
    modules: BTreeMap<CanonicalizedModulePath, ModuleConfig>,
    // This is used to lazily cache the result of sourcemap::SourceMap::from_slice across
    // modules and functions. There are certain source maps whose source origin we don't
    // need to construct during analysis (i.e. if all of the UDFs it defines have function
    // bodies outside the current module), so keeping this mapping lazy allows for avoiding
    // unnecessary source map parsing.
    source_maps_cache: BTreeMap<CanonicalizedModulePath, Option<sourcemap::SourceMap>>,
    rng: ChaCha12Rng,
    unix_timestamp: UnixTimestamp,
    environment_variables: BTreeMap<EnvVarName, EnvVarValue>,
}

impl<RT: Runtime> IsolateEnvironment<RT> for AnalyzeEnvironment {
    type Rng = ChaCha12Rng;

    fn trace(&mut self, message: String) -> anyhow::Result<()> {
        tracing::warn!("Unexpected Console access at import time: {message}");
        Ok(())
    }

    fn trace_system(&mut self, message: LogLine) -> anyhow::Result<()> {
        tracing::warn!(
            "Unexpected Console access at import time: {}",
            message.to_pretty_string()
        );
        Ok(())
    }

    fn rng(&mut self) -> anyhow::Result<&mut Self::Rng> {
        Ok(&mut self.rng)
    }

    fn unix_timestamp(&self) -> anyhow::Result<UnixTimestamp> {
        Ok(self.unix_timestamp)
    }

    fn get_environment_variable(
        &mut self,
        name: EnvVarName,
    ) -> anyhow::Result<Option<EnvVarValue>> {
        let value = self.environment_variables.get(&name).cloned();
        Ok(value)
    }

    fn get_table_mapping_without_system_tables(&mut self) -> anyhow::Result<TableMappingValue> {
        anyhow::bail!(ErrorMetadata::bad_request(
            "NoTableMappingFetchDuringImport",
            "Getting the table mapping unsupported at import time"
        ))
    }

    fn get_all_table_mappings(&mut self) -> anyhow::Result<(TableMapping, VirtualTableMapping)> {
        anyhow::bail!(ErrorMetadata::bad_request(
            "NoTableMappingFetchDuringImport",
            "Getting the table mapping unsupported at import time"
        ))
    }

    async fn lookup_source(
        &mut self,
        path: &str,
        _timeout: &mut Timeout<RT>,
        _permit: &mut Option<ConcurrencyPermit>,
    ) -> anyhow::Result<Option<(ModuleSource, Option<SourceMap>)>> {
        let p = ModulePath::from_str(path)?.canonicalize();
        let result = self
            .modules
            .get(&p)
            .cloned()
            .map(|config| (config.source, config.source_map));
        Ok(result)
    }

    fn syscall(&mut self, name: &str, _args: JsonValue) -> anyhow::Result<JsonValue> {
        match name {
            "count" | "get" | "insert" | "update" | "replace" | "queryStreamNext" | "queryPage"
            | "remove" => anyhow::bail!(ErrorMetadata::bad_request(
                "NoDbDuringImport",
                "Can't use database at import time"
            )),
            _ => anyhow::bail!(ErrorMetadata::bad_request(
                "NoSyscallDuringImport",
                format!("Syscall {name} unsupported at import time")
            )),
        }
    }

    fn start_async_syscall(
        &mut self,
        name: String,
        _args: JsonValue,
        _resolver: v8::Global<v8::PromiseResolver>,
    ) -> anyhow::Result<()> {
        anyhow::bail!(ErrorMetadata::bad_request(
            format!("No{}DuringImport", syscall_name_for_error(&name)),
            format!(
                "{} unsupported at import time",
                syscall_description_for_error(&name),
            ),
        ))
    }

    fn start_async_op(
        &mut self,
        request: AsyncOpRequest,
        _resolver: v8::Global<v8::PromiseResolver>,
    ) -> anyhow::Result<()> {
        anyhow::bail!(ErrorMetadata::bad_request(
            format!("No{}DuringImport", request.name_for_error()),
            format!(
                "{} unsupported at import time",
                request.description_for_error()
            ),
        ))
    }

    fn user_timeout(&self) -> std::time::Duration {
        *DATABASE_UDF_USER_TIMEOUT
    }

    fn system_timeout(&self) -> std::time::Duration {
        *DATABASE_UDF_SYSTEM_TIMEOUT
    }
}

impl AnalyzeEnvironment {
    pub async fn analyze<RT: Runtime>(
        isolate: &mut Isolate<RT>,
        udf_config: UdfConfig,
        modules: BTreeMap<CanonicalizedModulePath, ModuleConfig>,
        environment_variables: BTreeMap<EnvVarName, EnvVarValue>,
    ) -> anyhow::Result<Result<BTreeMap<CanonicalizedModulePath, AnalyzedModule>, JsError>> {
        let to_analyze = modules
            .keys()
            .filter(|p| !p.is_deps())
            .cloned()
            .collect::<Vec<_>>();
        anyhow::ensure!(
            modules
                .values()
                .all(|m| m.environment == ModuleEnvironment::Isolate),
            "Isolate environment can only analyze Isolate modules"
        );
        let rng = ChaCha12Rng::from_seed(udf_config.import_phase_rng_seed);
        let unix_timestamp = udf_config.import_phase_unix_timestamp;
        let environment = AnalyzeEnvironment {
            modules,
            source_maps_cache: BTreeMap::new(),
            rng,
            unix_timestamp,
            environment_variables,
        };
        let (handle, state) = isolate.start_request(environment).await?;
        let mut handle_scope = isolate.handle_scope();
        let v8_context = v8::Context::new(&mut handle_scope);
        let mut context_scope = v8::ContextScope::new(&mut handle_scope, v8_context);
        let mut isolate_context =
            RequestScope::new(&mut context_scope, handle.clone(), state, false).await?;
        let handle = isolate_context.handle();
        let result = Self::run_analyze(&mut isolate_context, to_analyze).await;

        // Drain the microtask queue, to clean up the isolate.
        isolate_context.scope.perform_microtask_checkpoint();

        // Unlink the request from the isolate.
        // After this point, it's unsafe to run js code in the isolate that
        // expects the current request's environment.
        // If the microtask queue is somehow nonempty after this point but before
        // the next request starts, the isolate may panic.
        drop(isolate_context);

        // Suppress the original error if the isolate was forcibly terminated.
        if let Err(e) = handle.take_termination_error()? {
            return Ok(Err(e));
        }
        result
    }

    fn get_source_map(
        &mut self,
        path: &CanonicalizedModulePath,
    ) -> anyhow::Result<&Option<sourcemap::SourceMap>> {
        match self.source_maps_cache.entry(path.clone()) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let module_config = self
                    .modules
                    .get(path)
                    .ok_or(anyhow!("could not find module config in environment"))?;
                let source_map = module_config
                    .source_map
                    .as_ref()
                    .map(|m| sourcemap::SourceMap::from_slice(m.as_bytes()))
                    .transpose()?;

                // cache it
                Ok(e.insert(source_map))
            },
        }
    }

    async fn run_analyze<RT: Runtime>(
        isolate: &mut RequestScope<'_, '_, RT, Self>,
        to_analyze: Vec<CanonicalizedModulePath>,
    ) -> anyhow::Result<Result<BTreeMap<CanonicalizedModulePath, AnalyzedModule>, JsError>> {
        let mut v8_scope = isolate.scope();
        let mut scope = RequestScope::<RT, Self>::enter(&mut v8_scope);

        // Iterate through modules paths to_analyze
        let mut result = BTreeMap::new();
        for module_path in to_analyze {
            // module_specifier is the key in the ModuleMap which we use to address the
            // ModuleId for this module. We then use this ModuleId to fetch the
            // v8::Module for evaluation.
            let module_specifier = module_specifier_from_path(&module_path)?;
            // Register the module and its dependencies with V8, instantiate the module, and
            // evaluate the module. After this, we can inspect the module's
            // in-memory objects to find functions which we can analyze as UDFs.
            // For more info on registration/instantiation see here: https://choubey.gitbook.io/internals-of-deno/import-and-ops/registration-and-instantiation
            let module: v8::Local<v8::Module> = match scope.eval_module(&module_specifier).await {
                Ok(m) => m,
                Err(e) => {
                    if let Some(e) = e.downcast_ref::<ModuleNotFoundError>() {
                        return Ok(Err(JsError::from_message(format!("{e}"))));
                    }
                    if let Some(e) = e.downcast_ref::<ModuleResolutionError>() {
                        return Ok(Err(JsError::from_message(format!("{e}"))));
                    }
                    match e.downcast::<JsError>() {
                        Ok(e) => {
                            return Ok(Err(JsError {
                                message: format!(
                                    "Failed to analyze {}: {}",
                                    module_path.as_str(),
                                    e.message
                                ),
                                custom_data: None,
                                frames: e.frames,
                            }))
                        },
                        Err(e) => return Err(e),
                    }
                },
            };

            // Gather UDFs, HTTP action routes, and crons
            let functions = match udf_analyze(&mut scope, &module, &module_path)? {
                Err(e) => return Ok(Err(e)),
                Ok(funcs) => WithHeapSize::from(funcs),
            };

            let mut http_routes = None;
            if module_path.is_http() {
                let routes = match http_analyze(&mut scope, &module, &module_path)? {
                    Err(err) => {
                        return Ok(Err(err));
                    },
                    Ok(value) => value,
                };
                http_routes = Some(WithHeapSize::from(routes));
            }

            let mut cron_specs = None;
            if module_path.is_cron() {
                let crons = match cron_analyze(&mut scope, &module, &module_path)? {
                    Err(err) => {
                        return Ok(Err(err));
                    },
                    Ok(value) => value,
                };
                cron_specs = Some(WithHeapSize::from(crons));
            }

            // Get source_index of current module
            let source_index = scope
                .state_mut()?
                .environment
                .get_source_map(&module_path)?
                .as_ref()
                .and_then(|source_map| {
                    for (i, filename) in source_map.sources().enumerate() {
                        if Path::new(filename).file_stem()
                            != Path::new(module_specifier.path()).file_stem()
                        {
                            continue;
                        }

                        return source_map.get_source_contents(i as u32).map(|_| i as u32);
                    }
                    None
                });

            let analyzed_module = AnalyzedModule {
                functions: functions.clone(),
                http_routes: http_routes.clone(),
                cron_specs: cron_specs.clone(),
                // source_mapped should be deprecated and migrated away from in the future
                source_mapped: Some(MappedModule {
                    source_index,
                    functions,
                    http_routes,
                    cron_specs,
                }),
            };
            result.insert(module_path, analyzed_module);
        }

        Ok(Ok(result))
    }
}

fn make_str_val<'s>(
    scope: &mut HandleScope<'s, ()>,
    value: &str,
) -> anyhow::Result<v8::Local<'s, v8::Value>> {
    let v8_str_val: v8::Local<v8::Value> = v8::String::new(scope, value)
        .ok_or_else(|| anyhow!("Failed to create v8 string for {}", value))?
        .into();
    Ok(v8_str_val)
}

fn udf_analyze<RT: Runtime>(
    scope: &mut ExecutionScope<RT, AnalyzeEnvironment>,
    module: &v8::Local<v8::Module>,
    module_path: &CanonicalizedModulePath,
) -> anyhow::Result<Result<Vec<AnalyzedFunction>, JsError>> {
    let namespace = module
        .get_module_namespace()
        .to_object(scope)
        .ok_or_else(|| anyhow!("Module namespace wasn't an object?"))?;
    let property_names = namespace
        .get_property_names(scope, GetPropertyNamesArgs::default())
        .ok_or_else(|| anyhow!("Failed to get module namespace property names"))?;

    // Iterate the properties and get the ones that are valid UDFs
    let mut functions = vec![];
    for i in 0..property_names.length() {
        let property_name = property_names
            .get_index(scope, i)
            .ok_or_else(|| anyhow!("Failed to get index {} on property names", i))?;
        let property_value = namespace
            .get(scope, property_name)
            .ok_or_else(|| anyhow!("Failed to get property name on module namespace"))?;
        let function: v8::Local<v8::Function> = match property_value.try_into() {
            Ok(f) => f,
            Err(_) => continue,
        };

        let property_name: v8::Local<v8::String> = property_name.try_into()?;
        let property_name = helpers::to_rust_string(scope, &property_name)?;

        let is_query_property = strings::isQuery.create(scope)?.into();
        let is_query: bool = function.has(scope, is_query_property).unwrap_or(false);

        let is_mutation_property = strings::isMutation.create(scope)?.into();
        let is_mutation: bool = function.has(scope, is_mutation_property).unwrap_or(false);

        let is_action_property = strings::isAction.create(scope)?.into();
        let is_action: bool = function.has(scope, is_action_property).unwrap_or(false);

        let udf_type = match (is_query, is_mutation, is_action) {
            (true, false, false) => UdfType::Query,
            (false, true, false) => UdfType::Mutation,
            (false, false, true) => UdfType::Action,
            _ => {
                tracing::warn!(
                    "Skipping function export that is not a mutation, query, or action: {} => \
                     ({is_query}, {is_mutation}, {is_action})",
                    property_name
                );
                continue;
            },
        };

        let is_public_property = strings::isPublic.create(scope)?.into();
        let is_public = function.has(scope, is_public_property).unwrap_or(false);

        let is_internal_property = strings::isInternal.create(scope)?.into();
        let is_internal = function.has(scope, is_internal_property).unwrap_or(false);

        // Call `exportArgs` to get the args validator.
        let export_args = strings::exportArgs.create(scope)?;
        let args = if let Some(export_args_value) = function.get(scope, export_args.into()) {
            if export_args_value.is_function() {
                let export_args_function: v8::Local<v8::Function> = export_args_value.try_into()?;
                let result_v8 = match scope
                    .with_try_catch(|s| export_args_function.call(s, function.into(), &[]))??
                {
                    Some(r) => v8::Local::<v8::String>::try_from(r)?,
                    None => {
                        anyhow::bail!("Missing return value from successful function call")
                    },
                };
                let result_str = helpers::to_rust_string(scope, &result_v8)?;
                match serde_json::from_str::<JsonValue>(&result_str) {
                    Ok(args_json) => match ArgsValidator::try_from(args_json) {
                        Ok(validator) => validator,
                        Err(parse_error) => {
                            let message =
                                format!("Unable to parse JSON from `exportArgs`: {parse_error}");
                            return Ok(Err(JsError::from_message(message)));
                        },
                    },
                    Err(json_error) => {
                        let message = format!(
                            "Unable to parse JSON returned from `exportArgs`: {json_error}"
                        );
                        return Ok(Err(JsError::from_message(message)));
                    },
                }
            } else if export_args_value.is_undefined() {
                // `exportArgs` will be undefined if this is before npm
                // package v0.13.0. Default to `Unvalidated`.
                ArgsValidator::Unvalidated
            } else {
                let message = "`exportArgs` is not a function or `undefined`.".to_string();
                return Ok(Err(JsError::from_message(message)));
            }
        } else {
            ArgsValidator::Unvalidated
        };

        let visibility = match (is_public, is_internal) {
            (true, false) => Some(Visibility::Public),
            (false, true) => Some(Visibility::Internal),
            (false, false) => None,
            (true, true) => {
                tracing::warn!(
                    "Skipping function export that is marked both public and internal: {}",
                    property_name
                );
                continue;
            },
        };

        // These are originally zero-indexed, so we just add 1
        let lineno = function
            .get_script_line_number()
            .ok_or_else(|| anyhow!("Failed to get function line number"))?
            + 1;
        let linecol = function
            .get_script_column_number()
            .ok_or_else(|| anyhow!("Failed to get function column number"))?
            + 1;

        // Get the appropriate source map to look in
        let (fn_source_map, fn_canon_path) = {
            let resource_name_val = function
                .get_script_origin()
                .resource_name()
                .ok_or(anyhow!("resource_name was None"))?;
            let resource_name = resource_name_val.to_rust_string_lossy(scope);
            let resource_url = module_specifier_from_str(&resource_name)?;
            let canon_path: CanonicalizedModulePath = path_from_module_specifier(&resource_url)?;

            (
                scope.state_mut()?.environment.get_source_map(&canon_path)?,
                canon_path,
            )
        };

        let canonicalized_name = FunctionName::from_untrusted(&property_name)?;
        if let Some(Some(token)) = fn_source_map.as_ref().map(|sm| sm.lookup_token(lineno, linecol))
            // This condition is in place so that we don't have to jump to source in source mappings
            // to get back to the original source. This logic gets complicated and is not strictly necessary now
            && fn_canon_path.as_str() == module_path.as_str()
        {
            // Source map is valid; proceed with mapping in original source map
            functions.push(AnalyzedFunction {
                name: canonicalized_name.clone(),
                pos: Some(AnalyzedSourcePosition {
                    path: fn_canon_path,
                    start_lineno: token.get_src_line(),
                    start_col: token.get_src_col(),
                }),
                udf_type,
                visibility: visibility.clone(),
                args: args.clone(),
            });
        } else {
            // If there is no valid source map, push a function without a position
            functions.push(AnalyzedFunction {
                name: canonicalized_name.clone(),
                pos: None,
                udf_type,
                visibility: visibility.clone(),
                args: args.clone(),
            });

            // Log reason for fallback
            if fn_canon_path.as_str() != module_path.as_str() {
                log_source_map_origin_in_separate_module();
            } else if fn_source_map.is_none() {
                log_source_map_missing();
            } else {
                log_source_map_token_lookup_failed();
            }

            tracing::warn!(
                "Failed to resolve source position of {module_path:?}:{canonicalized_name}"
            );
        }
    }

    // Sort by line number where source position of None compares least
    functions.sort_by(|a, b| a.pos.cmp(&b.pos));

    Ok(Ok(functions))
}

/// The `convex/http.js` default export, must be an HTTP router. In addition to
/// normal module analysis, this module may contain a Vec of
/// `AnalyzedHttpRoute`s returned by `Router.getRoutes()` which is currently
/// used only by the dashboard for dispaying HTTP routes. These routes are
/// publicly accessible at domains like `https://happy-otter-123.convex.site`.
fn http_analyze<RT: Runtime>(
    scope: &mut ExecutionScope<RT, AnalyzeEnvironment>,
    module: &v8::Local<v8::Module>,
    module_path: &CanonicalizedModulePath,
) -> anyhow::Result<Result<Vec<AnalyzedHttpRoute>, JsError>> {
    let mut http_routes: Vec<AnalyzedHttpRoute> = vec![];

    let namespace = module
        .get_module_namespace()
        .to_object(scope)
        .ok_or_else(|| anyhow!("Module namespace wasn't an object?"))?;
    let property_names = namespace
        .get_property_names(scope, GetPropertyNamesArgs::default())
        .ok_or_else(|| anyhow!("Failed to get module namespace property names"))?;

    let mut default_property_name: Option<v8::Local<v8::Value>> = None;
    for i in 0..property_names.length() {
        let property_name_v8 = property_names
            .get_index(scope, i)
            .ok_or_else(|| anyhow!("Failed to get index {} on property names", i))?;
        let property_name: v8::Local<v8::String> = property_name_v8.try_into()?;
        let property_name = helpers::to_rust_string(scope, &property_name)?;
        if property_name == "default" {
            default_property_name = Some(property_name_v8);
        }
    }
    if default_property_name.is_none() {
        let message = "`convex/http.js` must have a default export of a Router.".to_string();
        return Ok(Err(JsError::from_message(message)));
    }
    let default_property_name = default_property_name.expect("no default property name");
    let router_value: v8::Local<v8::Value> = namespace
        .get(scope, default_property_name)
        .ok_or_else(|| anyhow!("Failed to get property name on module namespace"))?;

    let Some(router) = router_value.to_object(scope) else {
        let message = "The default export of `convex/http.js` is not a Router.".to_string();
        return Ok(Err(JsError::from_message(message)));
    };

    let is_router_str = make_str_val(scope, "isRouter")?;
    let get_routes_str = make_str_val(scope, "getRoutes")?;
    let length_str = make_str_val(scope, "length")?;

    let mut is_router = false;
    if let Some(true) = router.has(scope, is_router_str) {
        is_router = router
            .get(scope, is_router_str)
            .ok_or_else(|| anyhow!("Missing `isRouter`"))?
            .is_true();
    }

    if !is_router {
        let message = "The default export of `convex/http.js` is not a Router.".to_string();
        return Ok(Err(JsError::from_message(message)));
    }

    let get_routes = match router.get(scope, get_routes_str) {
        Some(get_routes) => {
            let get_routes: Result<v8::Local<v8::Function>, _> = get_routes.try_into();
            match get_routes {
                Ok(get_routes) => get_routes,
                Err(_) => {
                    let message = ".getRoutes property on Router not found".to_string();
                    return Ok(Err(JsError::from_message(message)));
                },
            }
        },
        None => {
            let message = ".get_routes of Router is not a function".to_string();
            return Ok(Err(JsError::from_message(message)));
        },
    };

    let global = scope.get_current_context().global(scope);

    // function get_routes(): [path: string, method: string, handler:
    // HttpAction][]
    let routes_arr = match get_routes.call(scope, global.into(), &[]) {
        Some(routes_arr) => {
            let routes_arr: Result<v8::Local<v8::Object>, _> = routes_arr.try_into();
            match routes_arr {
                Ok(routes_arr) => routes_arr,
                Err(_) => {
                    return routes_error("return value is not an array");
                },
            }
        },
        None => {
            return routes_error("no value returned");
        },
    };

    let Some(len): Option<v8::Local<v8::Value>> = routes_arr.get(scope, length_str) else {
        return routes_error("return value is not an array");
    };
    let len = len
        .int32_value(scope)
        .expect("length could not be converted to i32")
        .try_into()
        .expect("length could not be converted to u32");

    for i in 0..len {
        let Some(entry) = routes_arr.get_index(scope, i) else {
            return routes_error(format!("problem with arr[{}]", i).as_str());
        };
        let Some(entry) = entry.to_object(scope) else {
            return routes_error(format!("arr[{}] is not an object", i).as_str());
        };

        let Some(path) = entry.get_index(scope, 0) else {
            return routes_error(format!("problem with arr[{}][0]", i).as_str());
        };
        let path: Result<v8::Local<v8::String>, _> = path.try_into();
        let Ok(path) = path else {
            return routes_error(format!("arr[{}][0] is not a string", i).as_str());
        };
        let path: String = path.to_rust_string_lossy(scope);

        let Some(method) = entry.get_index(scope, 1) else {
            return routes_error(format!("problem with arr[{}][1]", i).as_str());
        };
        let method: Result<v8::Local<v8::String>, _> = method.try_into();
        let Ok(method) = method else {
            return routes_error(format!("arr[{}][1] is not a string", i).as_str());
        };
        let method: String = method.to_rust_string_lossy(scope);
        let Ok(method): Result<RoutableMethod, _> = method.parse() else {
            return routes_error(
                format!(
                    "'{}' is not not a routable method (one of GET, POST, PUT, DELETE, PATCH, \
                     OPTIONS)",
                    method
                )
                .as_str(),
            );
        };

        let Some(function) = entry.get_index(scope, 2) else {
            return routes_error(format!("problem with third element of {} of array", i).as_str());
        };
        let function: Result<v8::Local<v8::Function>, _> = function.try_into();
        let Ok(function) = function else {
            return routes_error(format!("arr[{}][2] not an HttpAction", i).as_str());
        };

        // These are originally zero-indexed, so we just add 1
        let lineno = function
            .get_script_line_number()
            .ok_or_else(|| anyhow!("Failed to get function line number"))?
            + 1;
        let linecol = function
            .get_script_column_number()
            .ok_or_else(|| anyhow!("Failed to get function column number"))?
            + 1;

        // Get the appropriate source map to look in
        let (fn_source_map, fn_canon_path) = {
            let resource_name_val = function
                .get_script_origin()
                .resource_name()
                .ok_or(anyhow!("resource_name was None"))?;
            let resource_name = resource_name_val.to_rust_string_lossy(scope);
            let resource_url = module_specifier_from_str(&resource_name)?;
            let canon_path: CanonicalizedModulePath = path_from_module_specifier(&resource_url)?;

            let source_map = scope.state_mut()?.environment.get_source_map(&canon_path)?;
            (source_map, canon_path)
        };

        let source_pos = fn_source_map
            .as_ref()
            .and_then(|sm| sm.lookup_token(lineno, linecol))
            .and_then(|token| {
                if fn_canon_path.as_str() == module_path.as_str() {
                    Some(AnalyzedSourcePosition {
                        path: fn_canon_path,
                        start_lineno: token.get_src_line(),
                        start_col: token.get_src_col(),
                    })
                } else {
                    None
                }
            });
        if source_pos.is_none() {
            tracing::warn!("Failed to resolve {module_path:?}:{path}");
        }
        http_routes.push(AnalyzedHttpRoute {
            route: HttpActionRoute {
                path: path.clone(),
                method,
            },
            pos: source_pos,
        });
    }

    // Sort by line number where source position of None compares least
    http_routes.sort_by(|a, b| a.pos.cmp(&b.pos));

    Ok(Ok(http_routes))
}

fn routes_error<OKType>(specific_error: &str) -> anyhow::Result<Result<OKType, JsError>> {
    let message = format!(
        "The `getRoutes()` method of Router did not return the expected type. `getRoutes()` \
         should be a function returning an array of entries of the form [path: string, method: \
         string, handler: HttpAction] ({specific_error})",
    );
    Ok(Err(JsError::from_message(message)))
}

/// The `convex/cron.js` default export must be a Crons object.
fn cron_analyze<RT: Runtime>(
    scope: &mut ExecutionScope<RT, AnalyzeEnvironment>,
    module: &v8::Local<v8::Module>,
    _module_path: &CanonicalizedModulePath,
) -> anyhow::Result<Result<BTreeMap<CronIdentifier, CronSpec>, JsError>> {
    let namespace = module
        .get_module_namespace()
        .to_object(scope)
        .ok_or_else(|| anyhow!("Module namespace wasn't an object?"))?;
    let property_names = namespace
        .get_property_names(scope, GetPropertyNamesArgs::default())
        .ok_or_else(|| anyhow!("Failed to get module namespace property names"))?;

    let mut default_property_name: Option<v8::Local<v8::Value>> = None;
    for i in 0..property_names.length() {
        let property_name_v8 = property_names
            .get_index(scope, i)
            .ok_or_else(|| anyhow!("Failed to get index {} on property names", i))?;
        let property_name: v8::Local<v8::String> = property_name_v8.try_into()?;
        let property_name = helpers::to_rust_string(scope, &property_name)?;
        if property_name == "default" {
            default_property_name = Some(property_name_v8);
        }
    }
    if default_property_name.is_none() {
        let message = "`convex/crons.js` must have a default export of a Crons object.".to_string();
        return Ok(Err(JsError::from_message(message)));
    }
    let default_property_name = default_property_name.expect("no default property name");
    let crons_value: v8::Local<v8::Value> = namespace
        .get(scope, default_property_name)
        .ok_or_else(|| anyhow!("Failed to get property name on module namespace"))?;

    let Some(crons) = crons_value.to_object(scope) else {
        let message = "The default export of `convex/cron.js` is not a Router.".to_string();
        return Ok(Err(JsError::from_message(message)));
    };

    let is_crons_str = make_str_val(scope, "isCrons")?;
    let export_str = make_str_val(scope, "export")?;

    let mut is_crons = false;
    if let Some(true) = crons.has(scope, is_crons_str) {
        is_crons = crons
            .get(scope, is_crons_str)
            .ok_or_else(|| anyhow!("Missing `isCrons`"))?
            .is_true();
    }

    if !is_crons {
        let message = "The default export of `convex/crons.js` is not a Crons object.".to_string();
        return Ok(Err(JsError::from_message(message)));
    }

    let export_function = match crons.get(scope, export_str) {
        Some(export) => {
            let export: Result<v8::Local<v8::Function>, _> = export.try_into();
            match export {
                Ok(export) => export,
                Err(_) => {
                    let message = ".export property on Crons object not found".to_string();
                    return Ok(Err(JsError::from_message(message)));
                },
            }
        },
        None => {
            let message = ".export of Crons object is not a function".to_string();
            return Ok(Err(JsError::from_message(message)));
        },
    };

    let result_v8 = match export_function.call(scope, crons.into(), &[]) {
        Some(r) => v8::Local::<v8::String>::try_from(r)?,
        None => anyhow::bail!("Missing return value from successful function call"),
    };
    let result_str = helpers::to_rust_string(scope, &result_v8)?;
    let export_json: BTreeMap<String, JsonValue> = serde_json::from_str(&result_str)?;

    let export_json = export_json;

    let mut cron_specs = BTreeMap::new();

    for (k, v) in export_json {
        let Ok(identifier) = k.parse() else {
            return Ok(Err(JsError::from_message(format!(
                "{} is not a valid Cron Identifier. Use only ASCII numbers, letters, spaces, \
                 underscores, dashes and apostrophes",
                k
            ))));
        };
        cron_specs.insert(identifier, CronSpec::try_from(v)?);
    }

    Ok(Ok(cron_specs))
}
