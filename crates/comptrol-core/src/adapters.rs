use crate::Risk;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AdapterDescriptor {
    pub name: String,
    pub version: String,
    pub platforms: Vec<String>,
    pub capabilities: Vec<String>,
    pub route: String,
    pub risk: Risk,
    pub isolation: String,
}

#[derive(Clone, Debug, Default)]
pub struct AdapterRegistry {
    descriptors: Vec<AdapterDescriptor>,
}

impl AdapterRegistry {
    pub fn builtin() -> Self {
        let all_desktop = || vec!["macos".to_owned(), "windows".to_owned(), "linux".to_owned()];
        let isolated = "out_of_process_loopback".to_owned();
        Self {
            descriptors: vec![
                AdapterDescriptor {
                    name: "comptrol.core".to_owned(),
                    version: "0.1".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "observe".to_owned(),
                        "policy".to_owned(),
                        "recovery".to_owned(),
                    ],
                    route: "native".to_owned(),
                    risk: Risk::R0,
                    isolation: "in_process_trusted".to_owned(),
                },
                AdapterDescriptor {
                    name: "comptrol.browser.cdp".to_owned(),
                    version: "0.1".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "target_discovery".to_owned(),
                        "accessibility_snapshot".to_owned(),
                        "evaluate".to_owned(),
                        "navigate".to_owned(),
                        "fill".to_owned(),
                        "click".to_owned(),
                        "wait_for".to_owned(),
                        "upload".to_owned(),
                        "download".to_owned(),
                        "open_tab".to_owned(),
                        "close_tab".to_owned(),
                        "history".to_owned(),
                    ],
                    route: "browser_protocol".to_owned(),
                    risk: Risk::R2,
                    isolation: "loopback_policy_bound".to_owned(),
                },
                AdapterDescriptor {
                    name: "comptrol.macos.ax".to_owned(),
                    version: "0.1".to_owned(),
                    platforms: vec!["macos".to_owned()],
                    capabilities: vec![
                        "press".to_owned(),
                        "set_value".to_owned(),
                        "postcondition".to_owned(),
                    ],
                    route: "macos_ax".to_owned(),
                    risk: Risk::R2,
                    isolation: "osascript_bounded".to_owned(),
                },
                AdapterDescriptor {
                    name: "comptrol.macos.launch".to_owned(),
                    version: "0.1".to_owned(),
                    platforms: vec!["macos".to_owned()],
                    capabilities: vec!["open_app".to_owned()],
                    route: "launchservices".to_owned(),
                    risk: Risk::R2,
                    isolation: "argument_vector_bounded".to_owned(),
                },
                AdapterDescriptor {
                    name: "comptrol.vscode".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "vscode.workspace.list".to_owned(),
                        "vscode.setting.get".to_owned(),
                        "vscode.setting.set".to_owned(),
                        "vscode.document.open".to_owned(),
                        "vscode.document.save".to_owned(),
                    ],
                    route: "isolated_vscode_bridge".to_owned(),
                    risk: Risk::R2,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.libreoffice".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "libreoffice.document.open".to_owned(),
                        "libreoffice.document.save".to_owned(),
                        "libreoffice.calc.range.read".to_owned(),
                        "libreoffice.calc.range.write".to_owned(),
                        "libreoffice.writer.text.replace".to_owned(),
                        "libreoffice.document.export".to_owned(),
                    ],
                    route: "isolated_libreoffice_uno".to_owned(),
                    risk: Risk::R2,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.obs".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "obs.scene.list".to_owned(),
                        "obs.scene.switch".to_owned(),
                        "obs.source.visibility.set".to_owned(),
                        "obs.recording.status".to_owned(),
                        "obs.recording.start".to_owned(),
                        "obs.recording.stop".to_owned(),
                    ],
                    route: "isolated_obs_websocket".to_owned(),
                    risk: Risk::R3,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.blender".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "blender.scene.object.list".to_owned(),
                        "blender.scene.object.create".to_owned(),
                        "blender.scene.object.transform".to_owned(),
                        "blender.project.save".to_owned(),
                        "blender.render".to_owned(),
                    ],
                    route: "isolated_blender_offline_typed_script".to_owned(),
                    risk: Risk::R2,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.resolve".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "video.project.list".to_owned(),
                        "video.project.open".to_owned(),
                        "video.project.create".to_owned(),
                        "video.project.save".to_owned(),
                        "video.media.import".to_owned(),
                        "video.media.bin.create".to_owned(),
                        "video.media.list".to_owned(),
                        "video.timeline.list".to_owned(),
                        "video.timeline.open".to_owned(),
                        "video.timeline.create".to_owned(),
                        "video.timeline.items.list".to_owned(),
                        "video.timeline.append".to_owned(),
                        "video.timeline.insert".to_owned(),
                        "video.timeline.batch".to_owned(),
                        "video.timeline.marker.add".to_owned(),
                        "video.timeline.marker.delete".to_owned(),
                        "video.timeline.item.properties.get".to_owned(),
                        "video.timeline.item.properties.set".to_owned(),
                        "video.render.preset.list".to_owned(),
                        "video.render.configure".to_owned(),
                        "video.render.add_job".to_owned(),
                        "video.render.start".to_owned(),
                        "video.render.status".to_owned(),
                        "video.render.cancel".to_owned(),
                    ],
                    route: "isolated_davinci_resolve".to_owned(),
                    risk: Risk::R2,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.google-workspace".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "document.google.read".to_owned(),
                        "document.google.batch_edit".to_owned(),
                        "document.text.insert".to_owned(),
                        "document.text.replace".to_owned(),
                        "document.text.style".to_owned(),
                        "document.export".to_owned(),
                        "presentation.google.read".to_owned(),
                        "presentation.slide.create".to_owned(),
                        "presentation.slide.delete".to_owned(),
                        "presentation.slide.reorder".to_owned(),
                        "presentation.text.replace".to_owned(),
                        "presentation.text.style".to_owned(),
                        "presentation.export".to_owned(),
                    ],
                    route: "isolated_google_workspace".to_owned(),
                    risk: Risk::R2,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.powerpoint".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "presentation.read".to_owned(),
                        "presentation.batch_edit".to_owned(),
                        "presentation.export".to_owned(),
                    ],
                    route: "isolated_powerpoint_openxml".to_owned(),
                    risk: Risk::R2,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.powerpoint-windows".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: vec!["windows".to_owned()],
                    capabilities: vec![
                        "presentation.desktop.open".to_owned(),
                        "presentation.slide.create".to_owned(),
                        "presentation.slide.delete".to_owned(),
                        "presentation.slide.reorder".to_owned(),
                        "presentation.shape.text.set".to_owned(),
                        "presentation.save".to_owned(),
                        "presentation.export_pdf".to_owned(),
                    ],
                    route: "isolated_powerpoint_com".to_owned(),
                    risk: Risk::R2,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.discord".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "discord.message.draft".to_owned(),
                        "discord.message.send".to_owned(),
                        "discord.message.edit".to_owned(),
                        "discord.message.delete".to_owned(),
                        "discord.message.reply".to_owned(),
                        "discord.message.react".to_owned(),
                        "discord.message.attach".to_owned(),
                        "discord.message.search".to_owned(),
                    ],
                    route: "isolated_discord_bot_api".to_owned(),
                    risk: Risk::R3,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.gmail".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "mail.draft".to_owned(),
                        "mail.send".to_owned(),
                        "mail.search".to_owned(),
                        "mail.read".to_owned(),
                    ],
                    route: "isolated_gmail_api".to_owned(),
                    risk: Risk::R3,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.microsoft-graph-mail".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "mail.draft".to_owned(),
                        "mail.send".to_owned(),
                        "mail.search".to_owned(),
                        "mail.read".to_owned(),
                    ],
                    route: "isolated_microsoft_graph_mail".to_owned(),
                    risk: Risk::R3,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.apple-mail".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: vec!["macos".to_owned()],
                    capabilities: vec![
                        "mail.draft".to_owned(),
                        "mail.send".to_owned(),
                        "mail.search".to_owned(),
                        "mail.read".to_owned(),
                    ],
                    route: "isolated_apple_mail".to_owned(),
                    risk: Risk::R3,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.apple-messages".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: vec!["macos".to_owned()],
                    capabilities: vec!["message.draft".to_owned(), "message.send".to_owned()],
                    route: "isolated_apple_messages".to_owned(),
                    risk: Risk::R3,
                    isolation: isolated.clone(),
                },
                AdapterDescriptor {
                    name: "comptrol.canva".to_owned(),
                    version: "0.1.0".to_owned(),
                    platforms: all_desktop(),
                    capabilities: vec![
                        "design.list".to_owned(),
                        "design.read".to_owned(),
                        "design.page.list".to_owned(),
                        "design.element.inspect".to_owned(),
                        "design.text.update".to_owned(),
                        "design.image.insert".to_owned(),
                        "design.element.create".to_owned(),
                        "design.element.delete".to_owned(),
                        "design.element.group".to_owned(),
                        "design.export".to_owned(),
                    ],
                    route: "isolated_canva_connect".to_owned(),
                    risk: Risk::R2,
                    isolation: isolated,
                },
            ],
        }
    }

    pub fn register(&mut self, descriptor: AdapterDescriptor) -> bool {
        if !descriptor.is_valid()
            || self
                .descriptors
                .iter()
                .any(|item| item.name == descriptor.name)
        {
            return false;
        }
        self.descriptors.push(descriptor);
        true
    }

    pub fn list(&self) -> &[AdapterDescriptor] {
        &self.descriptors
    }
}

impl AdapterDescriptor {
    pub fn is_valid(&self) -> bool {
        !self.name.is_empty()
            && !self.version.is_empty()
            && !self.route.is_empty()
            && !self.isolation.is_empty()
            && self
                .name
                .chars()
                .all(|character| !character.is_control() && !character.is_whitespace())
            && !self.platforms.is_empty()
            && !self.capabilities.is_empty()
            && self
                .platforms
                .iter()
                .chain(self.capabilities.iter())
                .all(|value| !value.is_empty() && !value.chars().any(char::is_control))
    }
}
