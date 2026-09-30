//! On-demand request schemas shared by MCP discovery and pre-dispatch checks.

use serde_json::{Value, json};

pub fn schema_for(intent: &str) -> Option<Value> {
    if let Some(schema) = browser_session_schema(intent).or_else(|| blender_schema(intent)) {
        return Some(schema);
    }
    if let Some(schema) = app_or_browser_schema(intent) {
        return Some(schema);
    }
    if let Some(schema) = core_schema(intent) {
        return Some(schema);
    }
    let selector = json!({"type":"string","minLength":1});
    let mut properties = json!({
        "process_id": {"type":"integer","minimum":1},
        "window_handle": {"type":"integer","minimum":1},
        "name": selector,
        "automation_id": selector,
        "role": {"type":"string","enum":["button","checkbox","combobox","listitem","option","edit","textfield","hyperlink"]},
        "value": {"type":"string"},
        "match_index": {"type":"integer","minimum":0},
        "expected_match_count": {"type":"integer","minimum":0},
        "max_nodes": {"type":"integer","minimum":1,"maximum":512},
        "key_sequence": {"type":"array","minItems":1,"maxItems":64,"items":{"type":"string","minLength":1}}
    });
    let (description, example, params_schema) = match intent {
        "windows.uia.inspect" => {
            properties
                .as_object_mut()
                .expect("object properties")
                .remove("value");
            properties
                .as_object_mut()
                .expect("object properties")
                .remove("key_sequence");
            properties
                .as_object_mut()
                .expect("object properties")
                .remove("match_index");
            properties
                .as_object_mut()
                .expect("object properties")
                .remove("expected_match_count");
            (
                "Observe controls in one exact process and optional exact window. An empty result is incomplete/unverified.",
                json!({"process_id":1234,"window_handle":5678,"max_nodes":80}),
                json!({"type":"object","required":["process_id"],"properties":properties,"additionalProperties":false}),
            )
        }
        "windows.uia.press" => {
            properties
                .as_object_mut()
                .expect("object properties")
                .remove("value");
            properties
                .as_object_mut()
                .expect("object properties")
                .remove("max_nodes");
            (
                "Invoke one unique named or AutomationId control semantically; physical click requires an explicit foreground posture. Key sequences require an exact window.",
                json!({"process_id":1234,"window_handle":5678,"name":"Save","role":"button"}),
                json!({"type":"object","required":["process_id"],"properties":properties,"anyOf":[{"required":["name"]},{"required":["automation_id"]},{"required":["key_sequence"]}],"dependentRequired":{"match_index":["expected_match_count"],"expected_match_count":["match_index"],"key_sequence":["window_handle"]},"additionalProperties":false}),
            )
        }
        "windows.uia.set_value" => {
            properties
                .as_object_mut()
                .expect("object properties")
                .remove("key_sequence");
            properties
                .as_object_mut()
                .expect("object properties")
                .remove("max_nodes");
            (
                "Set one named or AutomationId control through its UIA Value pattern.",
                json!({"process_id":1234,"window_handle":5678,"automation_id":"SearchBox","value":"query"}),
                json!({"type":"object","required":["process_id","value"],"properties":properties,"anyOf":[{"required":["name"]},{"required":["automation_id"]}],"dependentRequired":{"match_index":["expected_match_count"],"expected_match_count":["match_index"]},"additionalProperties":false}),
            )
        }
        "workflow.speculate" => {
            let step = json!({"type":"object","required":["intent"],"properties":{
                "intent": {"type":"string","minLength":1},
                "params": {"type":"object"},
                "postcondition": {"type":"object"},
                "rollback_hint": {"type":"object"}
            },"additionalProperties":false});
            (
                "Run N intent steps optimistically in ONE operate call with per-step postconditions and rollback hints. Each step re-enters the normal dispatch path with its own classification and consent gates. On failure the result carries the durable state of every executed step plus a reconcile plan instead of an opaque error.",
                json!({"steps":[
                    {"intent":"system.ping","params":{},"postcondition":{"equals":{"ready":true}}},
                    {"intent":"app.launch","params":{"app":"calc"},"rollback_hint":{"intent":"app.close","params":{"app":"calc"}}}
                ],"deadline_ms":30000}),
                json!({"type":"object","required":["steps"],"properties":{
                    "steps":{"type":"array","minItems":1,"maxItems":64,"items":step},
                    "deadline_ms":{"type":"integer","minimum":1000,"maximum":300000}
                },"additionalProperties":false}),
            )
        }
        _ => return None,
    };
    Some(json!({
        "intent": intent,
        "description": description,
        "params": params_schema,
        "postcondition": {
            "type":"object",
            "properties": {
                "attribute":{"type":"string","enum":["name","value","enabled","selected"]},
                "equals":{"type":"string"}
            },
            "required":["attribute","equals"],
            "additionalProperties":false
        },
        "example":{"intent":intent,"params":example}
    }))
}

fn browser_session_schema(intent: &str) -> Option<Value> {
    let (description, example, params) = match intent {
        "browser.session.list" => (
            "Discover supported local browser sessions without connecting to them.",
            json!({}),
            json!({"type":"object","properties":{},"additionalProperties":false}),
        ),
        "browser.session.connect" => (
            "Connect to one provider returned by browser.session.list; connection consent is still enforced by the provider.",
            json!({"provider":"companion_extension"}),
            json!({"type":"object","required":["provider"],"properties":{"provider":{"type":"string","enum":["chrome_permissioned_auto_connect","companion_extension","explicit_cdp_endpoint"]}},"additionalProperties":false}),
        ),
        _ => return None,
    };
    Some(
        json!({"intent":intent,"description":description,"params":params,"example":{"intent":intent,"params":example}}),
    )
}

fn blender_schema(intent: &str) -> Option<Value> {
    let number = json!({"type":"number","minimum":-1000000,"maximum":1000000});
    let unit = json!({"type":"number","minimum":0,"maximum":1});
    let vector3 = json!({"type":"array","minItems":3,"maxItems":3,"items":number});
    let color = json!({"type":"array","minItems":3,"maxItems":4,"items":unit});
    let path = json!({"type":"string","minLength":1});
    let (description, example, params) = match intent {
        "blender.scene.object.list" => (
            "List objects in the exact input project.",
            json!({"input_path":"C:/work/scene.blend"}),
            json!({"type":"object","required":["input_path"],"properties":{"input_path":path},"additionalProperties":false}),
        ),
        "blender.scene.object.create" => (
            "Create one named primitive in the input project and save the result to a new .blend output_path.",
            json!({"input_path":"C:/work/source.blend","output_path":"C:/work/result.blend","name":"Cube","shape":"cube"}),
            json!({"type":"object","required":["input_path","output_path","name"],"properties":{"input_path":path,"output_path":path,"name":{"type":"string","minLength":1,"maxLength":120},"shape":{"type":"string","enum":["cube","uv_sphere","ico_sphere","cylinder","cone","torus","plane"]},"location":vector3,"rotation":vector3,"scale":vector3,"base_color":color,"metallic":unit,"roughness":unit},"additionalProperties":false}),
        ),
        "blender.scene.object.transform" => (
            "Transform one named object in the input project and write to a new .blend output_path.",
            json!({"input_path":"C:/work/source.blend","output_path":"C:/work/result.blend","name":"Cube","location":[1,0,0]}),
            json!({"type":"object","required":["input_path","output_path","name"],"properties":{"input_path":path,"output_path":path,"name":{"type":"string","minLength":1},"location":vector3,"rotation":vector3,"scale":vector3,"base_color":color,"metallic":unit,"roughness":unit},"additionalProperties":false}),
        ),
        "blender.scene.object.delete" => (
            "Delete one named object from the input project and save to a new .blend output_path.",
            json!({"input_path":"C:/work/source.blend","output_path":"C:/work/result.blend","name":"Cube"}),
            json!({"type":"object","required":["input_path","output_path","name"],"properties":{"input_path":path,"output_path":path,"name":{"type":"string","minLength":1}},"additionalProperties":false}),
        ),
        "blender.scene.create_2d_rocket" => (
            "Create and verify a new 2D rocket .blend project and PNG render.",
            json!({"output_path":"C:/work/rocket.blend","render_path":"C:/work/rocket.png"}),
            json!({"type":"object","required":["output_path"],"properties":{"output_path":path,"render_path":path},"additionalProperties":false}),
        ),
        "blender.scene.copy_2d_rocket_to_3d" => (
            "Convert an exact existing 2D rocket project into a new 3D .blend copy.",
            json!({"input_path":"C:/work/rocket.blend","output_path":"C:/work/rocket-3d.blend","depth":0.35,"render_evidence":false}),
            json!({"type":"object","required":["input_path","output_path"],"properties":{"input_path":path,"output_path":path,"depth":{"type":"number","minimum":0.02,"maximum":10},"render_evidence":{"type":"boolean"}},"additionalProperties":false}),
        ),
        "blender.project.save" => (
            "Save the current Blender project to one explicit .blend path.",
            json!({"path":"C:/work/project.blend"}),
            json!({"type":"object","required":["path"],"properties":{"path":path},"additionalProperties":false}),
        ),
        "blender.render" => (
            "Render one frame from an exact input project to an explicit image or video artifact.",
            json!({"input_path":"C:/work/project.blend","output_path":"C:/work/render.png","frame":1}),
            json!({"type":"object","required":["input_path","output_path"],"properties":{"input_path":path,"output_path":path,"frame":{"type":"integer","minimum":0}},"additionalProperties":false}),
        ),
        _ => return None,
    };
    Some(
        json!({"intent":intent,"description":description,"params":params,"example":{"intent":intent,"params":example}}),
    )
}

fn app_or_browser_schema(intent: &str) -> Option<Value> {
    let string = |min_length| json!({"type":"string","minLength":min_length});
    let resource = json!({"oneOf":[
        {"type":"object","required":["kind","path"],"properties":{"kind":{"type":"string","const":"file"},"path":string(1)},"additionalProperties":false},
        {"type":"object","required":["kind","url"],"properties":{"kind":{"type":"string","const":"url"},"url":string(1)},"additionalProperties":false},
        {"type":"object","required":["kind","uri"],"properties":{"kind":{"type":"string","const":"deep_link"},"uri":string(1)},"additionalProperties":false},
        {"type":"object","required":["kind"],"properties":{"kind":{"type":"string","const":"none"}},"additionalProperties":false}
    ]});
    let (description, example, params) = match intent {
        "app.launch" | "app.focus" | "app.open_resource" => {
            let mut properties = json!({
                "app": string(1),
                "resource": resource,
                "settle_ms": {"type":"integer","minimum":200,"maximum":2000},
                // P5.3/P5.4: correlate the launch with the window that
                // appears, and choose the state it is left in. `hidden`
                // lets a desktop app be driven through UIA while the user
                // sees nothing change.
                "window_state": {"type":"string","enum":["normal","hidden","minimized","off_desktop"]},
                "surface_timeout_ms": {"type":"integer","minimum":200,"maximum":30000}
            });
            let required = if intent == "app.open_resource" {
                vec!["app", "resource"]
            } else {
                vec!["app"]
            };
            let props = properties.as_object_mut().expect("object schema");
            if intent == "app.focus" {
                props.retain(|key, _| key == "app");
                props.insert(
                    "window_handle".to_owned(),
                    json!({"type":"integer","minimum":1}),
                );
            } else if intent == "app.launch" {
                props.insert(
                    "window_handle".to_owned(),
                    json!({"type":"integer","minimum":1}),
                );
                props.insert(
                    "readiness_timeout_ms".to_owned(),
                    json!({"type":"integer","minimum":100,"maximum":30000}),
                );
                props.insert(
                    "instance_policy".to_owned(),
                    json!({"type":"string","enum":["reuse_unique","launch_new","error_if_running"]}),
                );
            }
            (
                if intent == "app.focus" {
                    "Focus one already-running application by exact registered identity. Provide window_handle when that app has multiple windows."
                } else {
                    "Resolve one exact installed application identity and request launch or open one typed resource."
                },
                if intent == "app.open_resource" {
                    json!({"app":"<exact app id>","resource":{"kind":"file","path":"C:/workspace/document.txt"}})
                } else if intent == "app.focus" {
                    json!({"app":"<exact app id>","window_handle":123456})
                } else {
                    json!({"app":"<exact app id>","instance_policy":"reuse_unique","readiness_timeout_ms":10000})
                },
                json!({"type":"object","required":required,"properties":properties,"additionalProperties":false}),
            )
        }
        "browser.cdp.open_tab" => (
            "Open one URL in the connected browser session; background controls whether the created tab is foregrounded.",
            json!({"url":"https://example.com/","background":true,"browser_context_id":"default"}),
            json!({"type":"object","required":["url"],"properties":{"url":string(1),"background":{"type":"boolean"},"browser_context_id":string(1)},"additionalProperties":false}),
        ),
        "browser.cdp.navigate" => (
            "Navigate one exact browser target and verify the resulting URL. Target and browser context identities are required.",
            json!({"target_id":"page-target-id","browser_context_id":"default","url":"https://example.com/"}),
            json!({"type":"object","required":["target_id","browser_context_id","url"],"properties":{"target_id":string(1),"browser_context_id":string(1),"url":string(1),"revision":string(1),"url_contains":{"type":"string"},"ready_expression":{"type":"string"},"skip_if_current":{"type":"boolean"}},"additionalProperties":false}),
        ),
        "browser.cdp.semantic_click" => (
            "Click one element resolved from a data-only semantic locator. The element is re-resolved inside the action transaction; ambiguous matches are refused before dispatch. Attestation is actor_dispatch; pair with browser.cdp.wait_for for independent outcome verification.",
            json!({"target_id":"page-target-id","browser_context_id":"default","revision":"target-revision","locator":{"role":"link","name":"Investment Club"},"timeout_ms":5000}),
            json!({"type":"object","required":["target_id","browser_context_id","locator"],"properties":{"target_id":string(1),"browser_context_id":string(1),"revision":string(1),"locator":{"oneOf":[{"type":"object","required":["role","name"],"properties":{"role":string(1),"name":string(1)},"additionalProperties":false},{"type":"object","required":["text"],"properties":{"text":string(1)},"additionalProperties":false},{"type":"object","required":["selector"],"properties":{"selector":string(1)},"additionalProperties":false},{"type":"object","required":["test_id"],"properties":{"test_id":string(1)},"additionalProperties":false},{"type":"object","required":["href_contains"],"properties":{"href_contains":string(1)},"additionalProperties":false}]},"timeout_ms":{"type":"integer","minimum":100,"maximum":30000}},"additionalProperties":false}),
        ),
        "browser.cdp.semantic_fill" => (
            "Fill one editable element resolved from a data-only semantic locator with an exact value, then verify the value from an in-page readback.",
            json!({"target_id":"page-target-id","browser_context_id":"default","revision":"target-revision","locator":{"role":"textbox","name":"Search"},"value":"query text","timeout_ms":5000}),
            json!({"type":"object","required":["target_id","browser_context_id","locator","value"],"properties":{"target_id":string(1),"browser_context_id":string(1),"revision":string(1),"locator":{"oneOf":[{"type":"object","required":["role","name"],"properties":{"role":string(1),"name":string(1)},"additionalProperties":false},{"type":"object","required":["selector"],"properties":{"selector":string(1)},"additionalProperties":false},{"type":"object","required":["test_id"],"properties":{"test_id":string(1)},"additionalProperties":false}]},"value":{"type":"string","maxLength":262144},"timeout_ms":{"type":"integer","minimum":100,"maximum":30000}},"additionalProperties":false}),
        ),
        "browser.cdp.discovery" => (
            "List live browser targets (tabs) with exact ids, URLs, titles, and revisions. Always observe before acting.",
            json!({"url_contains":"classroom.google.com"}),
            json!({"type":"object","properties":{"url_contains":string(1),"title_contains":string(1)},"additionalProperties":false}),
        ),
        "browser.cdp.wait_for" => (
            "Wait until a bounded postcondition on one exact target is observed true, verified from a fresh target observation.",
            json!({"target_id":"page-target-id","browser_context_id":"default","url_contains":"/c/MjAwNz"}),
            json!({"type":"object","required":["target_id","browser_context_id"],"properties":{"target_id":string(1),"browser_context_id":string(1),"revision":string(1),"selector":string(1),"property":{"type":"string","enum":["textContent","value","title","href","checked","disabled","readyState"]},"equals":{"oneOf":[{"type":"string"},{"type":"boolean"},{"type":"number"}]},"contains":string(1),"url_contains":string(1),"text_contains":string(1),"ready_expression":string(1),"timeout_ms":{"type":"integer","minimum":100,"maximum":30000}},"anyOf":[{"required":["url_contains"]},{"required":["text_contains"]},{"required":["ready_expression"]},{"required":["selector","equals"]},{"required":["selector","contains"]}],"additionalProperties":false}),
        ),
        "browser.cdp.activate_tab" => (
            "Activate (foreground) or deactivate (background) one exact live tab. Activation is a disclosed foreground change for rendering-dependent steps; deactivate hands the foreground back afterward.",
            json!({"target_id":"page-target-id","browser_context_id":"default","revision":"target-revision","deactivate":false}),
            json!({"type":"object","required":["target_id","browser_context_id","revision"],"properties":{"target_id":string(1),"browser_context_id":string(1),"revision":string(1),"deactivate":{"type":"boolean"}},"additionalProperties":false}),
        ),
        "browser.cdp.compact_snapshot" => (
            "Read a small, bounded list of visible actionable controls from one exact browser target.",
            json!({"target_id":"page-target-id","browser_context_id":"default","revision":"target-revision","limit":64}),
            json!({"type":"object","required":["target_id","browser_context_id","revision"],"properties":{"target_id":string(1),"browser_context_id":string(1),"revision":string(1),"limit":{"type":"integer","minimum":1,"maximum":160}},"additionalProperties":false}),
        ),
        "browser.cdp.accessibility_snapshot" => (
            "Read a bounded accessibility tree from one exact browser target.",
            json!({"target_id":"page-target-id","browser_context_id":"default","revision":"target-revision","depth":8}),
            json!({"type":"object","required":["target_id","browser_context_id","revision"],"properties":{"target_id":string(1),"browser_context_id":string(1),"revision":string(1),"depth":{"type":"integer","minimum":1,"maximum":20}},"additionalProperties":false}),
        ),
        "browser.cdp.workflow" => {
            let timeout = json!({"type":"integer","minimum":100,"maximum":30000});
            let locator_string = |key: &str| json!({"type":"object","required":[key],"properties":{key:string(1)},"additionalProperties":false});
            let locator = json!({"oneOf":[
                {"type":"object","required":["role","name"],"properties":{"role":string(1),"name":string(1)},"additionalProperties":false},
                locator_string("label"), locator_string("placeholder"), locator_string("text"),
                locator_string("test_id"), locator_string("alt_text"), locator_string("href_contains"),
                locator_string("selector"),
                {"type":"object","required":["backend_node_id"],"properties":{"backend_node_id":{"type":"integer","minimum":1}},"additionalProperties":false}
            ]});
            let steps = json!({"type":"array","minItems":1,"maxItems":32,"items":{"oneOf":[
                {"type":"object","required":["action","url"],"properties":{"action":{"type":"string","const":"navigate"},"url":string(1),"url_contains":{"type":"string"},"timeout_ms":timeout},"additionalProperties":false},
                {"type":"object","required":["action","locator"],"properties":{"action":{"type":"string","const":"click"},"locator":locator,"timeout_ms":timeout},"additionalProperties":false},
                {"type":"object","required":["action","locator","value"],"properties":{"action":{"type":"string","const":"fill"},"locator":locator,"value":{"type":"string","maxLength":262144},"timeout_ms":timeout},"additionalProperties":false},
                {"type":"object","required":["action","contains"],"properties":{"action":{"type":"string","const":"wait_url"},"contains":string(1),"timeout_ms":timeout},"additionalProperties":false},
                {"type":"object","required":["action","text"],"properties":{"action":{"type":"string","const":"wait_text"},"text":string(1),"timeout_ms":timeout},"additionalProperties":false}
            ]}});
            (
                "Run a bounded sequence of data-only browser actions in one request. For Google Classroom account selection, bind to the account chooser page, click the exact school account, wait for the class list, click the exact class, and finish with a URL or text postcondition. Bind by exact target id or by a unique page URL/title matcher; ambiguous matches are refused before actions.",
                json!({"target_url_contains":"accounts.google.com","steps":[{"action":"wait_text","text":"prgauri@williamsvillek12.org"},{"action":"click","locator":{"text":"prgauri@williamsvillek12.org"}},{"action":"wait_text","text":"Investment Club"},{"action":"click","locator":{"role":"link","name":"Investment Club"}},{"action":"wait_url","contains":"/c/"}]}),
                json!({"type":"object","required":["steps"],"properties":{"target_id":string(1),"browser_context_id":string(1),"revision":{"type":"string"},"target_url_contains":string(1),"target_title_contains":string(1),"steps":steps},"anyOf":[{"required":["target_id"]},{"required":["target_url_contains"]}],"additionalProperties":false}),
            )
        }
        _ => return None,
    };
    Some(json!({
        "intent": intent,
        "description": description,
        "params": params,
        "example": {"intent": intent, "params": example}
    }))
}

pub fn validate_params(intent: &str, params: &Value) -> Result<(), String> {
    validate_params_inner(intent, params).map_err(|message| {
        schema_for(intent).map_or(message.clone(), |schema| {
            format!(
                "{message}; minimal example: {}",
                schema["example"]["params"]
            )
        })
    })
}

fn validate_params_inner(intent: &str, params: &Value) -> Result<(), String> {
    let Some(schema) = schema_for(intent) else {
        return Ok(());
    };
    // `Value::Null` is how the protocol represents "no parameters" (the
    // MCP envelope always carries a params key). Treating it as an empty
    // object keeps every no-argument intent callable; required fields are
    // still reported below.
    let object = match params {
        Value::Object(object) => object,
        Value::Null => return validate_params_inner(intent, &json!({})),
        _ => return Err("params: expected an object".to_owned()),
    };
    let properties = schema["params"]["properties"]
        .as_object()
        .expect("intent schemas define params properties");

    for required in schema["params"]["required"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let field = required.as_str().expect("required fields are strings");
        if !object.contains_key(field) {
            return Err(format!(
                "params.{field}: required; example: {}",
                schema["example"]["params"]
            ));
        }
    }
    for (field, value) in object {
        let Some(field_schema) = properties.get(field) else {
            return Err(format!("params.{field}: unknown field for {intent}"));
        };
        validate_value(field, value, field_schema)?;
    }

    if let Some(any_of) = schema["params"]["anyOf"].as_array()
        && !any_of.iter().any(|alternative| {
            alternative["required"].as_array().is_some_and(|fields| {
                fields.iter().all(|field| {
                    field
                        .as_str()
                        .is_some_and(|field| object.get(field).is_some_and(property_is_present))
                })
            })
        })
    {
        return Err(format!(
            "params: provide one of the required selector fields; example: {}",
            schema["example"]["params"]
        ));
    }
    if let Some(dependencies) = schema["params"]["dependentRequired"].as_object() {
        for (source, required_fields) in dependencies {
            if object.contains_key(source)
                && let Some(required_fields) = required_fields.as_array()
                && let Some(missing) = required_fields
                    .iter()
                    .find_map(|field| field.as_str().filter(|field| !object.contains_key(*field)))
            {
                return Err(format!(
                    "params.{missing}: required when using params.{source}"
                ));
            }
        }
    }
    Ok(())
}

pub fn available_schemas() -> Vec<&'static str> {
    let mut schemas = vec![
        "windows.uia.inspect",
        "windows.uia.press",
        "windows.uia.set_value",
        "app.launch",
        "app.focus",
        "app.open_resource",
        "browser.cdp.open_tab",
        "browser.cdp.navigate",
        "browser.cdp.compact_snapshot",
        "browser.cdp.accessibility_snapshot",
        "browser.cdp.workflow",
        "workflow.speculate",
        "browser.session.list",
        "browser.session.connect",
        "blender.scene.object.list",
        "blender.scene.object.create",
        "blender.scene.object.transform",
        "blender.scene.object.delete",
        "blender.scene.create_2d_rocket",
        "blender.scene.copy_2d_rocket_to_3d",
        "blender.project.save",
        "blender.render",
    ];
    // Everything else in the core surface is described by `core_schema`.
    // Deriving the list from the dispatch registry means a newly dispatched
    // intent is published automatically once it has a schema, and the
    // conformance test fails if it does not.
    for intent in crate::CORE_INTENTS {
        if core_schema(intent).is_some() && !schemas.contains(intent) {
            schemas.push(intent);
        }
    }
    schemas.sort_unstable();
    schemas.dedup();
    schemas
}

fn validate_value(field: &str, value: &Value, schema: &Value) -> Result<(), String> {
    if let Some(one_of) = schema["oneOf"].as_array() {
        let matches = one_of
            .iter()
            .filter(|variant| validate_value(field, value, variant).is_ok())
            .count();
        if matches != 1 {
            return Err(format!(
                "params.{field}: value must match exactly one allowed variant"
            ));
        }
        return Ok(());
    }
    let field_type = schema["type"].as_str().unwrap_or_default();
    let valid_type = match field_type {
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.as_f64().is_some_and(f64::is_finite),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "boolean" => value.is_boolean(),
        _ => false,
    };
    if !valid_type {
        return Err(format!("params.{field}: expected {field_type}"));
    }
    if let Some(number) = value.as_i64()
        && (schema["minimum"]
            .as_i64()
            .is_some_and(|minimum| number < minimum)
            || schema["minimum"]
                .as_u64()
                .is_some_and(|minimum| number < minimum as i64)
            || schema["maximum"]
                .as_i64()
                .is_some_and(|maximum| number > maximum)
            || schema["maximum"]
                .as_u64()
                .is_some_and(|maximum| number >= 0 && number as u64 > maximum))
    {
        return Err(format!(
            "params.{field}: value is outside the allowed range"
        ));
    }
    if let Some(number) = value.as_f64()
        && (schema["minimum"]
            .as_f64()
            .is_some_and(|minimum| number < minimum)
            || schema["maximum"]
                .as_f64()
                .is_some_and(|maximum| number > maximum))
    {
        return Err(format!(
            "params.{field}: value is outside the allowed range"
        ));
    }
    if let Some(text) = value.as_str() {
        if schema["minLength"]
            .as_u64()
            .is_some_and(|min| text.len() < min as usize)
        {
            return Err(format!("params.{field}: value must not be empty"));
        }
        if schema["maxLength"]
            .as_u64()
            .is_some_and(|max| text.len() > max as usize)
        {
            return Err(format!("params.{field}: value exceeds the maximum length"));
        }
        if let Some(choices) = schema["enum"].as_array()
            && !choices.iter().any(|choice| choice.as_str() == Some(text))
        {
            return Err(format!("params.{field}: unsupported value {text:?}"));
        }
        if schema["const"]
            .as_str()
            .is_some_and(|expected| expected != text)
        {
            return Err(format!(
                "params.{field}: value does not match the required constant"
            ));
        }
    }
    if let Some(items) = value.as_array() {
        if schema["minItems"]
            .as_u64()
            .is_some_and(|min| items.len() < min as usize)
            || schema["maxItems"]
                .as_u64()
                .is_some_and(|max| items.len() > max as usize)
        {
            return Err(format!(
                "params.{field}: item count is outside the allowed range"
            ));
        }
        let item_schema = &schema["items"];
        for (index, item) in items.iter().enumerate() {
            validate_value(&format!("{field}[{index}]"), item, item_schema)?;
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema["properties"].as_object();
        if let Some(required) = schema["required"].as_array()
            && let Some(missing) = required
                .iter()
                .find_map(|key| key.as_str().filter(|key| !object.contains_key(*key)))
        {
            return Err(format!("params.{field}.{missing}: required"));
        }
        for (key, value) in object {
            let Some(field_schema) = properties.and_then(|properties| properties.get(key)) else {
                if schema["additionalProperties"] == false {
                    return Err(format!("params.{field}.{key}: unknown field"));
                }
                continue;
            };
            validate_value(&format!("{field}.{key}"), value, field_schema)?;
        }
    }
    Ok(())
}

fn property_is_present(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::String(text) => !text.trim().is_empty(),
        _ => true,
    }
}

/// Schemas for the core (non-adapter) intent surface. Every intent in
/// `CORE_INTENTS` is covered: a client that only reads published schemas
/// must be able to find every route, and the conformance test in
/// `tests/conformance.rs` fails if one is missing or if an example here
/// does not validate against its own schema.
fn core_schema(intent: &str) -> Option<Value> {
    let s = |min_length| json!({"type":"string","minLength":min_length});
    let non_empty = s(1);
    let path = s(1);
    let identifier = s(1);
    let object = |properties: Value, required: &[&str]| json!({"type":"object","required":required,"properties":properties,"additionalProperties":false});
    // Every browser.cdp.* mutation is bound to one exact target, browser
    // context, and revision, so a target-bound subset is shared instead
    // of restated 25 times.
    let target_bound = |extra: Value, required: Vec<&str>| {
        let mut properties = json!({
            "target_id": identifier,
            "browser_context_id": identifier,
            "revision": {"type":"string","minLength":1}
        });
        let map = properties.as_object_mut().expect("object properties");
        for (key, value) in extra.as_object().expect("extra properties") {
            map.insert(key.clone(), value.clone());
        }
        object(properties, &required)
    };

    let (description, example, params) = match intent {
        // --- readiness and observation -------------------------------------
        "recipe.run" => (
            "Run one promoted, replay-gated recipe in a single call. Each step re-enters the normal dispatch path with its own classification, policy gate, and consent check, so a recipe is a shorter conversation rather than a wider door. An unpromoted recipe is refused with recipe_not_promoted instead of being run as if it were proven.",
            json!({"recipe":"recipe.classroom_open_class","parameters":{"account":"student@example.test","class_name":"Investment Club"}}),
            object(
                json!({
                    "recipe": {
                        "type":"string",
                        "enum":[
                            "recipe.classroom_open_class",
                            "recipe.espn_open_scoreboard",
                            "recipe.settings_toggle",
                            "recipe.calculator_multiply"
                        ]
                    },
                    "parameters": {"type":"object"}
                }),
                &["recipe"],
            ),
        ),
        "system.ping" => (
            "Local readiness check. Returns the protocol version and a runtime fingerprint with no side effects.",
            json!({}),
            object(json!({}), &[]),
        ),
        "desktop.observe" => (
            "Read-only host observation: visible windows, running applications, and per-process window detail. Nothing is focused or actuated.",
            json!({"visible_only":true,"max_windows":32}),
            object(
                json!({
                    "visible_only": {"type":"boolean"},
                    "max_windows": {"type":"integer","minimum":1,"maximum":512}
                }),
                &[],
            ),
        ),
        "platform.broker.observe" => (
            "Read-only diagnostics for one desktop automation broker. Never actuates.",
            json!({"broker":"windows_uia"}),
            object(
                json!({"broker":{"type":"string","enum":["windows_uia","macos_ax","linux_atspi"]}}),
                &["broker"],
            ),
        ),
        "permission.status" => (
            "Report current platform permission and consent state. Read-only.",
            json!({}),
            object(json!({}), &[]),
        ),
        "permission.request" => (
            "Report the exact local step needed for one permission, without silently changing it.",
            json!({"permission":"accessibility"}),
            object(
                json!({"permission":{"type":"string","enum":["accessibility","screen_recording","automation","notification"]}}),
                &["permission"],
            ),
        ),

        // --- workflows ------------------------------------------------------
        "workflow.execute" => (
            "Run a workflow in the bounded workflow VM. Either supply a compiled IR under compiled_workflow, or a flat ops array. Steps are data, never caller-supplied code.",
            json!({"ops":[
                {"op":"sense","key":"ready","value":true},
                {"op":"assert","key":"ready","equals":true},
                {"op":"return","value":{"done":true}}
            ]}),
            object(
                json!({
                    "ops": {"type":"array","minItems":1,"maxItems":32,"items":{"type":"object"}},
                    "compiled_workflow": {"type":"object"},
                    "parameters": {"type":"object"},
                    "observed": {"type":"object"}
                }),
                &["ops"],
            ),
        ),

        // --- filesystem ------------------------------------------------------
        "filesystem.write" => (
            "Write one file inside the configured sandbox root. Parent traversal is refused.",
            json!({"path":"notes.txt","content":"hello"}),
            object(json!({"path":path,"content":{"type":"string"}}), &["path"]),
        ),
        "filesystem.copy" => (
            "Copy one file inside the configured sandbox root and verify the result.",
            json!({"source":"a.txt","destination":"b.txt"}),
            object(
                json!({"source":path,"destination":path}),
                &["source", "destination"],
            ),
        ),
        "filesystem.restore_checkpoint" => (
            "Restore a sandbox file from a checkpoint created by an earlier write.",
            json!({"checkpoint":"ckpt-1"}),
            object(json!({"checkpoint":identifier}), &["checkpoint"]),
        ),

        // --- desktop ---------------------------------------------------------
        "desktop.terminal" => (
            "Run one or more allowlisted commands in a real terminal and read the output back. Commands are structured argv, never a shell string, so an argument cannot become a second command. Verify with a postcondition of {\"output_contains\": \"...\"} to get verified rather than unverified.",
            json!({"commands":[{"program":"git","args":["status","--short"]}],"cwd":"C:/work","visible":false,"timeout_ms":10000}),
            object(
                json!({
                    "commands": {
                        "type":"array",
                        "minItems":1,
                        "maxItems":32,
                        "items": object(
                            json!({
                                "program": non_empty,
                                "args": {"type":"array","maxItems":128,"items":s(0)},
                                "cwd": s(0)
                            }),
                            &["program"],
                        )
                    },
                    "cwd": path,
                    "visible": {"type":"boolean"},
                    "timeout_ms": {"type":"integer","minimum":100,"maximum":600000}
                }),
                &["commands"],
            ),
        ),
        "desktop.explorer" => (
            "Reveal one exact existing path in the platform file manager. This never mutates the file, and the result is unverified because Comptrol does not claim to know what is on screen afterwards.",
            json!({"path":"C:/work/report.pdf"}),
            object(json!({"path":path}), &["path"]),
        ),
        "desktop.notify" => (
            "Show one local desktop notification with an exact title and body.",
            json!({"title":"Build","body":"Finished"}),
            object(
                json!({"title":non_empty,"body":non_empty}),
                &["title", "body"],
            ),
        ),
        "desktop.open_app" => (
            "Open one registered application by exact app id or display name.",
            json!({"app":"Blender"}),
            object(json!({"app":non_empty}), &["app"]),
        ),
        "app.resolve" => (
            "Resolve one application selector to its exact registry entry without launching it.",
            json!({"app":"calc"}),
            object(json!({"app":non_empty}), &["app"]),
        ),
        "app.list" => (
            "List registered applications, optionally filtered by one query substring.",
            json!({"query":"blender"}),
            object(json!({"query":{"type":"string"}}), &[]),
        ),
        "app.close" => (
            "Reserved route for closing an application. Not implemented in this build; it refuses rather than pretending.",
            json!({"app":"Blender"}),
            object(json!({"app":non_empty}), &[]),
        ),

        // --- settings and software -------------------------------------------
        "settings.get" => (
            "Read one exact system or browser setting key.",
            json!({"key":"dark_mode"}),
            object(json!({"key":non_empty}), &["key"]),
        ),
        "settings.set" | "settings.write" => (
            "Write one exact system or browser setting key and read it back for verification. The value may be a string, number, or boolean depending on the key.",
            json!({"key":"settings.bluetooth.enabled","value":true}),
            object(
                json!({"key":non_empty,"value":{"oneOf":[{"type":"string"},{"type":"number"},{"type":"boolean"}]}}),
                &["key", "value"],
            ),
        ),
        "software.search" => (
            "Search one local software provider for installable packages.",
            json!({"provider":"winget","query":"blender"}),
            object(
                json!({"provider":{"type":"string","minLength":1},"query":{"type":"string"}}),
                &["query"],
            ),
        ),
        "software.describe" => (
            "Describe one exact package from a local provider without installing it.",
            json!({"package":"BlenderFoundation.Blender"}),
            object(json!({"package":non_empty}), &["package"]),
        ),
        "software.install" => (
            "Install one exact package. Long-running operations are resumed with resume_operation_id. Consent is checked before the package id is read, so an ungranted call is refused as consent_required rather than invalid_input.",
            json!({"package":"BlenderFoundation.Blender","accept_agreements":false}),
            object(
                json!({
                    "package":non_empty,
                    "version":s(0),
                    "source":s(0),
                    "provider":s(0),
                    "accept_agreements": {"type":"boolean"},
                    "resume_operation_id": identifier
                }),
                &[],
            ),
        ),
        "software.update" => (
            "Update one exact installed package, resuming a long-running operation when given its id.",
            json!({"package":"BlenderFoundation.Blender"}),
            object(
                json!({"package":non_empty,"resume_operation_id":identifier}),
                &[],
            ),
        ),
        "software.uninstall" => (
            "Uninstall one exact package, resuming a long-running operation when given its id.",
            json!({"package":"BlenderFoundation.Blender"}),
            object(
                json!({"package":non_empty,"resume_operation_id":identifier}),
                &[],
            ),
        ),
        "popup.inspect" => (
            "Classify one visible dialog or popup surface from its observed signals, without dismissing anything.",
            json!({"role":"dialog","name":"Checkout","text":"Enter payment details"}),
            object(
                json!({
                    "role": {"type":"string","minLength":1},
                    "name": {"type":"string"},
                    "text": {"type":"string"}
                }),
                &[],
            ),
        ),
        "popup.dismiss" => (
            "Plan a dismissal for one classified popup by class. Protected classes (auth, payment, security, privilege) are never auto-approved; the plan is returned for the caller to execute.",
            json!({"role":"dialog","name":"Tip of the day","text":"Did you know about this feature","dismiss_tips":true}),
            object(
                json!({
                    "role": {"type":"string","minLength":1},
                    "name": {"type":"string"},
                    "text": {"type":"string"},
                    "dismiss_cookie_banners": {"type":"boolean"},
                    "dismiss_informational": {"type":"boolean"},
                    "dismiss_tips": {"type":"boolean"},
                    "dismiss_update_prompts": {"type":"boolean"},
                    "close_actions": {"type":"array","maxItems":32,"items":s(1)}
                }),
                &[],
            ),
        ),
        "command.run" => (
            "Run one allowlisted executable with explicit argv inside an explicit root. Never invokes a shell; required policy is COMPTROL_ALLOW_COMMANDS plus COMPTROL_COMMAND_ROOT and COMPTROL_COMMAND_ALLOWLIST.",
            json!({"program":"git.exe","args":["status"],"cwd":"C:/work","timeout_ms":10000}),
            object(
                json!({
                    "program": non_empty,
                    "args": {"type":"array","maxItems":64,"items":s(0)},
                    "cwd": path,
                    "timeout_ms": {"type":"integer","minimum":1,"maximum":60000}
                }),
                &["program", "cwd"],
            ),
        ),

        // --- platform accessibility parity ------------------------------------
        "macos.ax.press" => (
            "Press one exact macOS Accessibility control semantically, with no synthesized pointer input.",
            json!({"process_id":1234,"control":"Save"}),
            json!({
                "type": "object",
                "required": ["process_id", "control"],
                "properties": {
                    "process_id": {"type":"integer","minimum":1},
                    "control": non_empty,
                    "automation_id": non_empty,
                    "role": non_empty,
                    "match_index": {"type":"integer","minimum":0},
                    "expected_match_count": {"type":"integer","minimum":0},
                    "max_nodes": {"type":"integer","minimum":1,"maximum":512}
                },
                "dependentRequired": {
                    "match_index": ["expected_match_count"],
                    "expected_match_count": ["match_index"]
                },
                "additionalProperties": false
            }),
        ),
        "macos.ax.set_value" => (
            "Set one exact macOS Accessibility control through its AX value setter.",
            json!({"process_id":1234,"control":"SearchBox","value":"query"}),
            json!({
                "type": "object",
                "required": ["process_id", "control", "value"],
                "properties": {
                    "process_id": {"type":"integer","minimum":1},
                    "control": non_empty,
                    "automation_id": non_empty,
                    "value": s(0),
                    "role": non_empty,
                    "match_index": {"type":"integer","minimum":0},
                    "expected_match_count": {"type":"integer","minimum":0},
                    "max_nodes": {"type":"integer","minimum":1,"maximum":512}
                },
                "dependentRequired": {
                    "match_index": ["expected_match_count"],
                    "expected_match_count": ["match_index"]
                },
                "additionalProperties": false
            }),
        ),
        "linux.atspi.press" => (
            "Press one exact Linux AT-SPI accessible by name or accessible-id in an exact process.",
            json!({"process_id":1234,"name":"Save"}),
            json!({
                "type": "object",
                "required": ["process_id"],
                "properties": {
                    "process_id": {"type":"integer","minimum":1},
                    "name": non_empty,
                    "automation_id": non_empty,
                    "role": non_empty,
                    "match_index": {"type":"integer","minimum":0},
                    "expected_match_count": {"type":"integer","minimum":0},
                    "max_nodes": {"type":"integer","minimum":1,"maximum":512}
                },
                "anyOf": [{"required":["name"]},{"required":["automation_id"]}],
                "dependentRequired": {
                    "match_index": ["expected_match_count"],
                    "expected_match_count": ["match_index"]
                },
                "additionalProperties": false
            }),
        ),
        "linux.atspi.set_value" => (
            "Set one exact Linux AT-SPI accessible by name or accessible-id through its text interface.",
            json!({"process_id":1234,"name":"SearchBox","value":"query"}),
            json!({
                "type": "object",
                "required": ["process_id", "value"],
                "properties": {
                    "process_id": {"type":"integer","minimum":1},
                    "name": non_empty,
                    "automation_id": non_empty,
                    "value": s(0),
                    "role": non_empty,
                    "match_index": {"type":"integer","minimum":0},
                    "expected_match_count": {"type":"integer","minimum":0},
                    "max_nodes": {"type":"integer","minimum":1,"maximum":512}
                },
                "anyOf": [{"required":["name"]},{"required":["automation_id"]}],
                "dependentRequired": {
                    "match_index": ["expected_match_count"],
                    "expected_match_count": ["match_index"]
                },
                "additionalProperties": false
            }),
        ),

        // --- browser: session, launcher, restore -----------------------------
        "browser.ensure_session" => (
            "One-call browser readiness probe: resolve a session and perform one bounded channel round trip. Read-only.",
            json!({}),
            object(json!({}), &[]),
        ),
        "browser.chrome.open_tab" => (
            "Open a URL in the user's own Chrome profile through the launcher route. May activate the window, so it refuses strict background posture.",
            json!({"url":"https://classroom.google.com"}),
            object(json!({"url":s(8)}), &["url"]),
        ),
        "browser.chrome.restore_recent" => (
            "Restore one exact recently closed Chrome item by kind. May activate the window, so it refuses strict background posture. For tab_group, urls reconstruct the group without retyping them.",
            json!({"kind":"tab_group","group":"Research","urls":["https://example.test/one"]}),
            object(
                json!({
                    "kind":{"type":"string","enum":["tab","window","tab_group"]},
                    "url": s(0),
                    "group": s(0),
                    "urls":{"type":"array","maxItems":32,"items":s(8)}
                }),
                &["kind"],
            ),
        ),
        "browser.chrome.reopen_closed_group" => (
            "Reopen one exact recently closed Chrome tab group by name. Foreground route; refuses strict background posture.",
            json!({"group":"Investment Club"}),
            object(json!({"group":non_empty}), &["group"]),
        ),
        "browser.fixture.submit" => (
            "Submit one message to the local browser fixture. Fixture-only mutation bound to exact target identity.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","message":"hello"}),
            object(
                json!({
                    "target_id": identifier,
                    "browser_context_id": identifier,
                    "revision": s(1),
                    "message": s(0)
                }),
                &["target_id", "browser_context_id", "revision", "message"],
            ),
        ),

        // --- browser.cdp: the one shared binding, many operations ------------
        "browser.cdp.discovery"
        | "browser.cdp.wait_for"
        | "browser.cdp.accessibility_snapshot"
        | "browser.cdp.screenshot"
        | "browser.cdp.close_tab"
        | "browser.cdp.history_back"
        | "browser.cdp.history_forward"
        | "browser.cdp.reopen_closed_group" => {
            let extra = if intent == "browser.cdp.screenshot" {
                json!({
                    "capture_id": identifier,
                    "format": {"type":"string","enum":["png","jpeg"]},
                    "quality": {"type":"integer","minimum":0,"maximum":100},
                    "include_pixels": {"type":"boolean"},
                    "clip": {"type":"object"},
                    "depth": {"type":"integer","minimum":1,"maximum":32}
                })
            } else {
                json!({})
            };
            let required: Vec<&str> = if intent == "browser.cdp.screenshot" {
                vec!["target_id", "browser_context_id", "revision"]
            } else {
                vec![]
            };
            (
                match intent {
                    "browser.cdp.discovery" => {
                        "List every open page target with its exact target id, browser context, and revision."
                    }
                    "browser.cdp.wait_for" => {
                        "Wait for one bounded condition on an exact target, using an allowlisted DOM property or a readiness expression."
                    }
                    "browser.cdp.accessibility_snapshot" => {
                        "Read the accessibility tree of one exact target with bounded depth."
                    }
                    "browser.cdp.screenshot" => {
                        "Capture one exact target by its capture id, with optional clip, format, and quality."
                    }
                    "browser.cdp.close_tab" => "Close one exact target.",
                    "browser.cdp.history_back" => {
                        "Navigate one exact target back in its own history."
                    }
                    "browser.cdp.history_forward" => {
                        "Navigate one exact target forward in its own history."
                    }
                    _ => {
                        "Reopen one exact recently closed Chrome group. Refuses strict background posture."
                    }
                },
                match intent {
                    "browser.cdp.discovery" => json!({}),
                    "browser.cdp.wait_for" => {
                        json!({"target_id":"t","browser_context_id":"c","revision":"r","selector":"document","property":"readyState","equals":"complete"})
                    }
                    "browser.cdp.accessibility_snapshot" => {
                        json!({"target_id":"t","browser_context_id":"c","revision":"r","depth":8})
                    }
                    "browser.cdp.screenshot" => {
                        json!({"target_id":"t","browser_context_id":"c","revision":"r","capture_id":"cap-1","format":"png"})
                    }
                    _ => json!({"target_id":"t","browser_context_id":"c","revision":"r"}),
                },
                target_bound(extra, required),
            )
        }
        "browser.cdp.navigate" => (
            "Navigate one exact target to an exact URL. Omit revision to bind at dispatch; supply it when the caller already observed that generation.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","url":"https://example.com","url_contains":"example.com"}),
            target_bound(
                json!({
                    "url": s(8),
                    "url_contains": s(0),
                    "ready_expression": s(0),
                    "skip_if_current": {"type":"boolean"},
                    "await_promise": {"type":"boolean"},
                    "timeout_ms": {"type":"integer","minimum":100,"maximum":120000}
                }),
                vec!["target_id", "browser_context_id", "url"],
            ),
        ),
        "browser.cdp.ensure_state" => (
            "Read the current target URL and optional readiness condition without navigating.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","url_contains":"example.com"}),
            target_bound(
                json!({"url":s(1),"url_contains":s(1),"ready_expression":s(1)}),
                vec!["target_id", "browser_context_id", "revision"],
            ),
        ),
        "browser.cdp.evaluate" => (
            "Evaluate one expression in an exact target, or assert one readiness expression, and report the observed result. Expression text is caller supplied; it never includes credentials.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","expression":"document.readyState","await_promise":true}),
            target_bound(
                json!({
                    "expression": non_empty,
                    "await_promise": {"type":"boolean"},
                    "ready_expression": s(0),
                    "url_contains": s(0)
                }),
                vec!["target_id", "browser_context_id", "expression"],
            ),
        ),
        "browser.cdp.frame_evaluate" => (
            "Evaluate one expression inside one exact frame, bound to the frame generation and revision observed with it.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","frame_id":"f","frame_generation":1,"frame_revision":2,"expression":"document.title"}),
            target_bound(
                json!({
                    "frame_id": non_empty,
                    "frame_generation": {"type":"integer","minimum":0},
                    "frame_revision": {"type":"integer","minimum":0},
                    "expression": non_empty,
                    "await_promise": {"type":"boolean"}
                }),
                vec![
                    "target_id",
                    "browser_context_id",
                    "frame_id",
                    "frame_generation",
                    "frame_revision",
                    "expression",
                ],
            ),
        ),
        "browser.cdp.semantic_click"
        | "browser.cdp.semantic_fill"
        | "browser.cdp.focus"
        | "browser.cdp.fill"
        | "browser.cdp.click" => {
            let extra = json!({
                "locator": {"type":"object"},
                "selector": s(0),
                "value": s(0),
                "text_contains": s(0),
                "timeout_ms": {"type":"integer","minimum":100,"maximum":120000}
            });
            let needs_value = intent.ends_with("fill");
            (
                match intent {
                    "browser.cdp.semantic_click" => {
                        "Click one exact element by semantic locator in an exact target, resolving through the accessibility tree rather than coordinates."
                    }
                    "browser.cdp.semantic_fill" => {
                        "Fill one exact element by semantic locator and verify the resulting value."
                    }
                    "browser.cdp.fill" => {
                        "Fill one exact target-bound element and verify the resulting value."
                    }
                    "browser.cdp.focus" => "Focus one exact element by semantic locator.",
                    _ => {
                        "Click one exact target-bound element, reporting unverified unless the caller supplies a postcondition."
                    }
                },
                if needs_value {
                    json!({"target_id":"t","browser_context_id":"c","revision":"r","locator":{"role":"link","name":"Investment Club"},"value":"text"})
                } else {
                    json!({"target_id":"t","browser_context_id":"c","revision":"r","locator":{"role":"link","name":"Investment Club"}})
                },
                target_bound(
                    extra,
                    if needs_value {
                        vec!["target_id", "browser_context_id", "locator", "value"]
                    } else {
                        vec!["target_id", "browser_context_id", "locator"]
                    },
                ),
            )
        }
        "browser.cdp.upload" | "browser.cdp.download" => (
            "Attach one local file to an exact target, or observe one exact target's downloads.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","path":"C:/work/file.txt"}),
            target_bound(
                json!({
                    "path": path,
                    "locator": {"type":"object"},
                    "timeout_ms": {"type":"integer","minimum":100,"maximum":120000}
                }),
                vec!["target_id", "browser_context_id"],
            ),
        ),
        "browser.cdp.coordinate_click" => (
            "Click one exact point inside an exact target. This is the only pixel route, it is R2, and it needs a screenshot proof target plus explicit consent.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","x":120,"y":240}),
            target_bound(
                json!({
                    "x": {"type":"integer","minimum":0},
                    "y": {"type":"integer","minimum":0},
                    "button": {"type":"string","enum":["left","middle","right"]},
                    "capture_id": identifier
                }),
                vec!["target_id", "browser_context_id", "x", "y"],
            ),
        ),
        "browser.cdp.type_text" => (
            "Type text into the element that currently has keyboard focus via real browser input events. Works in canvas editors (Google Docs, Sheets) where DOM writes never reach the model. Place the caret first with a semantic or coordinate click.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","text":"hello"}),
            target_bound(
                json!({"text": {"type":"string","minLength":1,"maxLength":1048576}}),
                vec!["target_id", "browser_context_id", "revision", "text"],
            ),
        ),
        "browser.cdp.press_key" => (
            "Press one named key via real browser input events, on the element with keyboard focus. Commits cell edits in spreadsheet editors (Enter), closes dialogs (Escape), and navigates (arrows, Tab).",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","key":"Enter","modifiers":["ctrl"]}),
            target_bound(
                json!({
                    "key": {"type":"string","minLength":1,"maxLength":32,"description":"Named key (Enter, Tab, Escape, Backspace, Delete, arrows, Home, End, PageUp, PageDown) or exactly one character"},
                    "modifiers": {"type":"array","items":{"type":"string","enum":["ctrl","control","alt","shift","meta","cmd","command","win","option","cmd_or_ctrl"]},"maxItems":4}
                }),
                vec!["target_id", "browser_context_id", "revision", "key"],
            ),
        ),
        "browser.cdp.dialog" => (
            "Report or hold one JavaScript dialog on an exact target. Comptrol never silently auto-answers a prompt.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","action":"dismiss"}),
            target_bound(
                json!({
                    "action": {"type":"string","enum":["accept","dismiss"]},
                    "prompt_text": {"type":"string"}
                }),
                vec!["target_id", "browser_context_id", "action"],
            ),
        ),
        "browser.cdp.open_tab" | "browser.cdp.activate_tab" => (
            "Open or activate one exact tab. `background: true` keeps it unfocused, which is the default for open_tab.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","url":"https://example.com","background":true}),
            target_bound(
                json!({
                    "url": s(0),
                    "background": {"type":"boolean"},
                    "deactivate": {"type":"boolean"}
                }),
                vec!["target_id", "browser_context_id"],
            ),
        ),
        "browser.cdp.compact_snapshot" => (
            "Bounded, replayable snapshot of one exact target: interactive elements with stable handles, so a later call can bind without a full re-read.",
            json!({"target_id":"t","browser_context_id":"c","revision":"r","limit":64,"depth":8}),
            target_bound(
                json!({
                    "limit": {"type":"integer","minimum":1,"maximum":512},
                    "depth": {"type":"integer","minimum":1,"maximum":32}
                }),
                vec!["target_id", "browser_context_id"],
            ),
        ),
        _ => return None,
    };
    Some(json!({
        "intent": intent,
        "description": description,
        "params": params,
        "example": {"intent": intent, "params": example}
    }))
}

#[cfg(test)]
mod tests {
    use super::{schema_for, validate_params};
    use serde_json::json;

    #[test]
    fn ui_automation_schema_is_shared_with_pre_dispatch_validation() {
        assert!(schema_for("windows.uia.press").is_some());
        assert!(
            validate_params(
                "windows.uia.press",
                &json!({"process_id":1234,"name":"Save","role":"button"})
            )
            .is_ok()
        );
        assert!(
            validate_params("windows.uia.press", &json!({"process_id":"1234"}))
                .unwrap_err()
                .contains("params.process_id: expected integer")
        );
        assert!(
            validate_params("windows.uia.press", &json!({"process_id":1234}))
                .unwrap_err()
                .contains("provide one of the required selector fields")
        );
        assert!(
            validate_params(
                "windows.uia.press",
                &json!({"process_id":1234,"name":"Save","match_index":0})
            )
            .unwrap_err()
            .contains("expected_match_count")
        );
        assert!(
            validate_params(
                "windows.uia.inspect",
                &json!({"process_id":1234,"unexpected":true})
            )
            .unwrap_err()
            .contains("unknown field")
        );
    }

    #[test]
    fn browser_and_app_schemas_validate_nested_typed_inputs() {
        assert!(schema_for("browser.cdp.workflow").is_some());
        for (intent, params) in [
            (
                "browser.cdp.compact_snapshot",
                json!({"target_id":"t","browser_context_id":"c","revision":"r","limit":64}),
            ),
            (
                "browser.cdp.accessibility_snapshot",
                json!({"target_id":"t","browser_context_id":"c","revision":"r","depth":8}),
            ),
        ] {
            assert!(schema_for(intent).is_some(), "{intent}");
            assert!(validate_params(intent, &params).is_ok(), "{intent}");
        }
        assert!(
            validate_params(
                "browser.cdp.workflow",
                &json!({
                    "target_url_contains":"classroom.google.com",
                    "target_title_contains":"Classroom",
                    "steps":[
                        {"action":"click","locator":{"role":"link","name":"Investment Club"}},
                        {"action":"wait_url","contains":"/c/"}
                    ]
                })
            )
            .is_ok()
        );
        assert!(
            validate_params(
                "browser.cdp.workflow",
                &json!({"steps":[{"action":"exec","code":"arbitrary"}]})
            )
            .is_err()
        );
        assert!(
            validate_params(
                "browser.cdp.workflow",
                &json!({"target_url_contains":"classroom.google.com","steps":[]})
            )
            .is_err()
        );
        assert!(
            validate_params(
                "browser.cdp.open_tab",
                &json!({"url":"https://example.com","background":true})
            )
            .is_ok()
        );
        assert!(
            validate_params(
                "browser.cdp.open_tab",
                &json!({"url":"https://example.com","background":"yes"})
            )
            .is_err()
        );
        assert!(
            validate_params(
                "browser.cdp.navigate",
                &json!({"target_id":"t","browser_context_id":"c","url":"https://example.com"})
            )
            .is_ok()
        );
        assert!(validate_params(
            "browser.cdp.navigate",
            &json!({"target_id":"t","browser_context_id":"c","revision":"url:https://example.com","url":"https://example.com"})
        ).is_ok());
        assert!(validate_params(
            "browser.cdp.navigate",
            &json!({"target_id":"t","browser_context_id":"c","revision":42,"url":"https://example.com"})
        ).is_err());
        assert!(
            validate_params(
                "browser.cdp.navigate",
                &json!({"target_id":"t","url":"https://example.com"})
            )
            .is_err()
        );
        assert!(
            validate_params(
                "app.open_resource",
                &json!({"app":"exact-id","resource":{"kind":"file","path":"C:/work/file.txt"}})
            )
            .is_ok(),
            "{:?}",
            validate_params(
                "app.open_resource",
                &json!({"app":"exact-id","resource":{"kind":"file","path":"C:/work/file.txt"}})
            )
        );
        assert!(
            validate_params(
                "app.open_resource",
                &json!({"app":"exact-id","resource":{"kind":"file","url":"https://example.com"}})
            )
            .is_err()
        );
        assert!(validate_params("app.launch", &json!({"app":"exact-id","settle_ms":199})).is_err());
        assert!(
            validate_params(
                "app.launch",
                &json!({"app":"calc","instance_policy":"guess"})
            )
            .is_err()
        );
        assert!(validate_params("app.focus", &json!({"app":"exact-id"})).is_ok());
        assert!(
            validate_params(
                "app.focus",
                &json!({"app":"exact-id","resource":{"kind":"file","path":"C:/work/file.txt"}})
            )
            .is_err()
        );
    }

    #[test]
    fn browser_dialog_schema_accepts_supported_actions() {
        assert!(
            validate_params(
                "browser.cdp.dialog",
                &json!({
                    "target_id":"t",
                    "browser_context_id":"c",
                    "revision":"r",
                    "action":"dismiss"
                })
            )
            .is_ok()
        );
        assert!(
            validate_params(
                "browser.cdp.dialog",
                &json!({
                    "target_id":"t",
                    "browser_context_id":"c",
                    "revision":"r",
                    "action":"dismiss",
                    "prompt_text":"answer"
                })
            )
            .is_ok()
        );
        assert!(
            validate_params(
                "browser.cdp.dialog",
                &json!({
                    "target_id":"t",
                    "browser_context_id":"c",
                    "revision":"r",
                    "action":"ignore"
                })
            )
            .is_err()
        );
    }

    #[test]
    fn session_and_blender_schemas_are_discoverable_and_validate_examples() {
        for intent in super::available_schemas() {
            let schema = schema_for(intent).expect("catalog entry has a schema");
            assert!(
                validate_params(intent, &schema["example"]["params"]).is_ok(),
                "{intent}"
            );
        }
        assert!(
            validate_params("browser.session.connect", &json!({"provider":"other"}))
                .unwrap_err()
                .contains("provider")
        );
        let error = validate_params("blender.scene.object.create", &json!({"input_path":"a.blend","output_path":"b.blend","name":"Cube","unexpected":true}))
            .unwrap_err();
        assert!(error.contains("params.unexpected"));
        assert!(error.contains("minimal example"));
        assert!(validate_params("blender.scene.object.create", &json!({"input_path":"a.blend","output_path":"b.blend","name":"Cube","location":[1,2]})).unwrap_err().contains("item count"));
    }

    #[test]
    fn stage_one_exit_workflows_validate_without_schema_probe_calls() {
        for (intent, params) in [
            ("app.launch", json!({"app":"calc"})),
            ("browser.session.list", json!({})),
            (
                "browser.session.connect",
                json!({"provider":"companion_extension"}),
            ),
            (
                "blender.scene.object.list",
                json!({"input_path":"C:/work/scene.blend"}),
            ),
        ] {
            assert!(validate_params(intent, &params).is_ok(), "{intent}");
        }
        let error = validate_params(
            "browser.cdp.workflow",
            &json!({
                "target_url_contains":"example.com",
                "steps":[{"action":"click","locator":{"role":"link"}}]
            }),
        )
        .unwrap_err();
        assert!(error.contains("locator"));
        assert!(error.contains("minimal example"));
        let navigation_error = validate_params(
            "browser.cdp.navigate",
            &json!({"url":"https://example.com"}),
        )
        .unwrap_err();
        assert!(navigation_error.contains("target_id"));
        assert!(navigation_error.contains("minimal example"));
    }
}
