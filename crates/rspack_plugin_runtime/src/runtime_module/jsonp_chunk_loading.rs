use std::ptr::NonNull;

use cow_utils::CowUtils;
use pollster::block_on;
use rspack_collections::{DatabaseItem, Identifier};
use rspack_core::{
  compile_boolean_matcher, impl_runtime_module,
  rspack_sources::{BoxSource, ConcatSource, RawStringSource, SourceExt},
  BooleanMatcher, Chunk, ChunkUkey, Compilation, CrossOriginLoading, RuntimeGlobals, RuntimeModule,
  RuntimeModuleStage,
};

use super::generate_javascript_hmr_runtime;
use crate::{
  get_chunk_runtime_requirements,
  runtime_module::utils::{chunk_has_js, get_initial_chunk_ids, stringify_chunks},
  LinkPrefetchData, LinkPreloadData, RuntimeModuleChunkWrapper, RuntimePlugin,
};

#[impl_runtime_module]
#[derive(Debug)]
pub struct JsonpChunkLoadingRuntimeModule {
  id: Identifier,
  chunk: Option<ChunkUkey>,
}

impl Default for JsonpChunkLoadingRuntimeModule {
  fn default() -> Self {
    Self::with_default(
      Identifier::from("webpack/runtime/jsonp_chunk_loading"),
      None,
    )
  }
}

impl JsonpChunkLoadingRuntimeModule {
  fn generate_base_uri(&self, chunk: &Chunk, compilation: &Compilation) -> BoxSource {
    let base_uri = chunk
      .get_entry_options(&compilation.chunk_group_by_ukey)
      .and_then(|options| options.base_uri.as_ref())
      .and_then(|base_uri| serde_json::to_string(base_uri).ok())
      .unwrap_or_else(|| "document.baseURI || self.location.href".to_string());
    RawStringSource::from(format!("{} = {};\n", RuntimeGlobals::BASE_URI, base_uri)).boxed()
  }
}

impl RuntimeModule for JsonpChunkLoadingRuntimeModule {
  fn name(&self) -> Identifier {
    self.id
  }

  fn generate(&self, compilation: &Compilation) -> rspack_error::Result<BoxSource> {
    let chunk = compilation
      .chunk_by_ukey
      .expect_get(&self.chunk.expect("The chunk should be attached"));

    let runtime_requirements = get_chunk_runtime_requirements(compilation, &chunk.ukey());
    let with_base_uri = runtime_requirements.contains(RuntimeGlobals::BASE_URI);
    let with_loading = runtime_requirements.contains(RuntimeGlobals::ENSURE_CHUNK_HANDLERS);
    let with_on_chunk_load = runtime_requirements.contains(RuntimeGlobals::ON_CHUNKS_LOADED);
    let with_hmr = runtime_requirements.contains(RuntimeGlobals::HMR_DOWNLOAD_UPDATE_HANDLERS);
    let with_hmr_manifest = runtime_requirements.contains(RuntimeGlobals::HMR_DOWNLOAD_MANIFEST);
    let with_callback = runtime_requirements.contains(RuntimeGlobals::CHUNK_CALLBACK);
    let with_prefetch = runtime_requirements.contains(RuntimeGlobals::PREFETCH_CHUNK_HANDLERS);
    let with_preload = runtime_requirements.contains(RuntimeGlobals::PRELOAD_CHUNK_HANDLERS);
    let with_fetch_priority = runtime_requirements.contains(RuntimeGlobals::HAS_FETCH_PRIORITY);
    let cross_origin_loading = &compilation.options.output.cross_origin_loading;
    let script_type = &compilation.options.output.script_type;
    let charset = compilation.options.output.charset;

    let hooks = RuntimePlugin::get_compilation_hooks(compilation.id());

    let condition_map =
      compilation
        .chunk_graph
        .get_chunk_condition_map(&chunk.ukey(), compilation, chunk_has_js);
    let has_js_matcher = compile_boolean_matcher(&condition_map);
    let initial_chunks = get_initial_chunk_ids(self.chunk, compilation, chunk_has_js);

    let js_matcher = has_js_matcher.render("chunkId");

    let mut source = ConcatSource::default();

    if with_base_uri {
      source.add(self.generate_base_uri(chunk, compilation));
    }

    source.add(RawStringSource::from(format!(
      r#"
      // object to store loaded and loading chunks
      // undefined = chunk not loaded, null = chunk preloaded/prefetched
      // [resolve, reject, Promise] = chunk loading, 0 = chunk loaded
      var installedChunks = {}{};
      "#,
      match with_hmr {
        true => {
          let state_expression = format!("{}_jsonp", RuntimeGlobals::HMR_RUNTIME_STATE_PREFIX);
          format!("{} = {} || ", state_expression, state_expression)
        }
        false => "".to_string(),
      },
      &stringify_chunks(&initial_chunks, 0)
    )));

    if with_loading {
      let body = if matches!(has_js_matcher, BooleanMatcher::Condition(false)) {
        "installedChunks[chunkId] = 0;".to_string()
      } else {
        include_str!("runtime/jsonp_chunk_loading.js")
          .cow_replace("$JS_MATCHER$", &js_matcher)
          .cow_replace(
            "$MATCH_FALLBACK$",
            if matches!(has_js_matcher, BooleanMatcher::Condition(true)) {
              ""
            } else {
              "else installedChunks[chunkId] = 0;\n"
            },
          )
          .cow_replace(
            "$FETCH_PRIORITY$",
            if with_fetch_priority {
              ", fetchPriority"
            } else {
              ""
            },
          )
          .into_owned()
      };

      source.add(RawStringSource::from(format!(
        r#"
        {}.j = function (chunkId, promises{}) {{
          {body}
        }}
        "#,
        RuntimeGlobals::ENSURE_CHUNK_HANDLERS,
        if with_fetch_priority {
          ", fetchPriority"
        } else {
          ""
        },
      )));
    }

    if with_prefetch && !matches!(has_js_matcher, BooleanMatcher::Condition(false)) {
      let cross_origin = match cross_origin_loading {
        CrossOriginLoading::Disable => "".to_string(),
        CrossOriginLoading::Enable(_) => {
          format!("link.crossOrigin = {}", cross_origin_loading)
        }
      };
      let link_prefetch_code = r#"
    var link = document.createElement('link');
    $LINK_CHART_CHARSET$
    $CROSS_ORIGIN$
    if (__webpack_require__.nc) {
      link.setAttribute("nonce", __webpack_require__.nc);
    }
    link.rel = "prefetch";
    link.as = "script";
    link.href = __webpack_require__.p + __webpack_require__.u(chunkId);  
      "#
      .cow_replace(
        "$LINK_CHART_CHARSET$",
        if charset {
          "link.charset = 'utf-8';"
        } else {
          ""
        },
      )
      .cow_replace("$CROSS_ORIGIN$", cross_origin.as_str())
      .to_string();

      let chunk_ukey = self.chunk.expect("The chunk should be attached");
      let res = block_on(async {
        hooks
          .link_prefetch
          .call(LinkPrefetchData {
            code: link_prefetch_code,
            chunk: RuntimeModuleChunkWrapper {
              chunk_ukey,
              compilation_id: compilation.id(),
              compilation: NonNull::from(compilation),
            },
          })
          .await
      })?;

      source.add(RawStringSource::from(
        include_str!("runtime/jsonp_chunk_loading_with_prefetch.js")
          .cow_replace("$JS_MATCHER$", &js_matcher)
          .cow_replace("$LINK_PREFETCH$", &res.code)
          .into_owned(),
      ));
    }

    if with_preload && !matches!(has_js_matcher, BooleanMatcher::Condition(false)) {
      let cross_origin = match cross_origin_loading {
        CrossOriginLoading::Disable => "".to_string(),
        CrossOriginLoading::Enable(cross_origin_value) => {
          if cross_origin_value.eq("use-credentials") {
            "link.crossOrigin = \"use-credentials\";".to_string()
          } else {
            format!(
              r#"
              if (link.href.indexOf(window.location.origin + '/') !== 0) {{
                link.crossOrigin = {}
              }}
              "#,
              cross_origin_loading
            )
          }
        }
      };
      let script_type_link_pre = if script_type.eq("module") || script_type.eq("false") {
        "".to_string()
      } else {
        format!(
          "link.type = {}",
          serde_json::to_string(script_type).expect("invalid json tostring")
        )
      };
      let script_type_link_post = if script_type.eq("module") {
        "link.rel = \"modulepreload\";"
      } else {
        r#"
        link.rel = "preload";
        link.as = "script";
        "#
      };

      let link_preload_code = r#"
    var link = document.createElement('link');
    $LINK_CHART_CHARSET$
    $SCRIPT_TYPE_LINK_PRE$
    if (__webpack_require__.nc) {
      link.setAttribute("nonce", __webpack_require__.nc);
    }
    $SCRIPT_TYPE_LINK_POST$
    link.href = __webpack_require__.p + __webpack_require__.u(chunkId);
    $CROSS_ORIGIN$  
      "#
      .cow_replace(
        "$LINK_CHART_CHARSET$",
        if charset {
          "link.charset = 'utf-8';"
        } else {
          ""
        },
      )
      .cow_replace("$CROSS_ORIGIN$", cross_origin.as_str())
      .cow_replace("$SCRIPT_TYPE_LINK_PRE$", script_type_link_pre.as_str())
      .cow_replace("$SCRIPT_TYPE_LINK_POST$", script_type_link_post)
      .to_string();

      let chunk_ukey = self.chunk.expect("The chunk should be attached");
      let res = block_on(async {
        hooks
          .link_preload
          .call(LinkPreloadData {
            code: link_preload_code,
            chunk: RuntimeModuleChunkWrapper {
              chunk_ukey,
              compilation_id: compilation.id(),
              compilation: NonNull::from(compilation),
            },
          })
          .await
      })?;

      source.add(RawStringSource::from(
        include_str!("runtime/jsonp_chunk_loading_with_preload.js")
          .cow_replace("$JS_MATCHER$", &js_matcher)
          .cow_replace("$LINK_PRELOAD$", &res.code)
          .into_owned(),
      ));
    }

    if with_hmr {
      source.add(RawStringSource::from(
        include_str!("runtime/jsonp_chunk_loading_with_hmr.js")
          .cow_replace("$GLOBAL_OBJECT$", &compilation.options.output.global_object)
          .cow_replace(
            "$HOT_UPDATE_GLOBAL$",
            &serde_json::to_string(&compilation.options.output.hot_update_global)
              .expect("failed to serde_json::to_string(hot_update_global)"),
          )
          .into_owned(),
      ));
      source.add(RawStringSource::from(generate_javascript_hmr_runtime(
        "jsonp",
      )));
    }

    if with_hmr_manifest {
      source.add(RawStringSource::from_static(include_str!(
        "runtime/jsonp_chunk_loading_with_hmr_manifest.js"
      )));
    }

    if with_on_chunk_load {
      source.add(RawStringSource::from_static(include_str!(
        "runtime/jsonp_chunk_loading_with_on_chunk_load.js"
      )));
    }

    if with_callback || with_loading {
      let chunk_loading_global_expr = format!(
        r#"{}["{}"]"#,
        &compilation.options.output.global_object, &compilation.options.output.chunk_loading_global
      );
      source.add(RawStringSource::from(
        include_str!("runtime/jsonp_chunk_loading_with_callback.js")
          .cow_replace("$CHUNK_LOADING_GLOBAL_EXPR$", &chunk_loading_global_expr)
          .cow_replace(
            "$WITH_ON_CHUNK_LOAD$",
            match with_on_chunk_load {
              true => "return __webpack_require__.O(result);",
              false => "",
            },
          )
          .into_owned(),
      ));
    }

    Ok(source.boxed())
  }

  fn attach(&mut self, chunk: ChunkUkey) {
    self.chunk = Some(chunk);
  }

  fn stage(&self) -> RuntimeModuleStage {
    RuntimeModuleStage::Attach
  }
}
