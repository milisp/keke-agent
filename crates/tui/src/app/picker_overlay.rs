//! The model, provider, and MCP overlays.

use crate::transcript::Cell;

use super::App;

impl App {
    /// Open the model overlay, or say why there is nothing to open.
    ///
    /// A provider that published no list is not an empty menu: it is a session
    /// where keke has no grounds to refuse any name, so the person is told to
    /// type one rather than shown a box with nothing in it.
    pub fn open_model_picker(&mut self) {
        if self.models.is_empty() {
            self.transcript.push(Cell::Notice(self.model_list()));
            return;
        }
        let mut picker = crate::picker::Picker::new(crate::picker::PickerKind::Model);
        if let Some(at) = self.models.iter().position(|model| model.id == self.model) {
            picker.move_selection(at as isize, self.models.len());
        }
        self.picker = Some(picker);
    }

    /// Open the provider overlay, or say why there is nothing to open.
    ///
    /// A build whose registry was never handed over is the provider list's
    /// version of a provider that published no models: the person is told what
    /// is in force and asked to name one, not shown an empty box.
    pub fn open_provider_picker(&mut self) {
        if self.routes.is_empty() {
            self.transcript.push(Cell::Notice(self.provider_list()));
            return;
        }
        let mut picker = crate::picker::Picker::new(crate::picker::PickerKind::Provider);
        if let Some(at) = self
            .routes
            .iter()
            .position(|route| Some(&route.route) == self.provider.as_ref())
        {
            picker.move_selection(at as isize, self.routes.len());
        }
        self.picker = Some(picker);
    }

    /// Open the MCP overlay, or say why there is nothing to open.
    ///
    /// Nothing configured is not an empty box: it is a session where the answer
    /// is a command to run, so [`crate::mcp::nothing_configured`] says that instead.
    pub fn open_mcp_picker(&mut self) {
        if self.mcp.is_empty() {
            self.transcript
                .push(Cell::Notice(crate::mcp::nothing_configured()));
            return;
        }
        // Start on the first server that needs something done to it, since
        // that is what a person came here for.
        let mut picker = crate::picker::Picker::new(crate::picker::PickerKind::Mcp);
        if let Some(at) = self
            .mcp
            .iter()
            .position(|server| server.allowed && server.remote && !server.signed_in)
        {
            picker.move_selection(at as isize, self.mcp.len());
        }
        self.picker = Some(picker);
    }

    /// The MCP overlay, if that is the one that is open.
    #[must_use]
    pub fn mcp_picker(&self) -> Option<&crate::picker::Picker> {
        self.picker
            .as_ref()
            .filter(|picker| picker.kind() == crate::picker::PickerKind::Mcp)
    }

    /// The MCP rows the overlay is showing this frame, after its filter.
    #[must_use]
    pub fn picker_mcp(&self) -> Vec<&crate::mcp::McpServerStatus> {
        let Some(picker) = self.mcp_picker() else {
            return Vec::new();
        };
        self.mcp
            .iter()
            .filter(|server| picker.matches(*server))
            .collect()
    }

    /// The model overlay, if that is the one that is open.
    #[must_use]
    pub fn model_picker(&self) -> Option<&crate::picker::Picker> {
        self.picker
            .as_ref()
            .filter(|picker| picker.kind() == crate::picker::PickerKind::Model)
    }

    /// The provider overlay, if that is the one that is open.
    #[must_use]
    pub fn provider_picker(&self) -> Option<&crate::picker::Picker> {
        self.picker
            .as_ref()
            .filter(|picker| picker.kind() == crate::picker::PickerKind::Provider)
    }

    /// Whether either overlay has the keyboard.
    #[must_use]
    pub fn picker_open(&self) -> bool {
        self.picker.is_some()
    }

    /// The model rows the overlay is showing this frame, after its filter.
    #[must_use]
    pub fn picker_models(&self) -> Vec<&keke_provider_api::ModelInfo> {
        let Some(picker) = self.model_picker() else {
            return Vec::new();
        };
        self.models
            .iter()
            .filter(|model| picker.matches(*model))
            .collect()
    }

    /// The provider rows the overlay is showing this frame, after its filter.
    #[must_use]
    pub fn picker_providers(&self) -> Vec<&crate::picker::ProviderChoice> {
        let Some(picker) = self.provider_picker() else {
            return Vec::new();
        };
        self.routes
            .iter()
            .filter(|route| picker.matches(*route))
            .collect()
    }

    /// How many rows the open overlay is showing, whichever list it is.
    fn picker_rows(&self) -> usize {
        match self.picker.as_ref().map(crate::picker::Picker::kind) {
            Some(crate::picker::PickerKind::Model) => self.picker_models().len(),
            Some(crate::picker::PickerKind::Provider) => self.picker_providers().len(),
            Some(crate::picker::PickerKind::Mcp) => self.picker_mcp().len(),
            None => 0,
        }
    }

    /// Which row of the open overlay is highlighted.
    #[must_use]
    pub fn picker_selected(&self) -> usize {
        let count = self.picker_rows();
        self.picker
            .as_ref()
            .map_or(0, |picker| picker.selected(count))
    }

    pub(crate) fn move_picker_selection(&mut self, delta: isize) {
        let count = self.picker_rows();
        if let Some(picker) = &mut self.picker {
            picker.move_selection(delta, count);
        }
    }

    pub(crate) fn type_into_picker(&mut self, ch: char) {
        if let Some(picker) = &mut self.picker {
            picker.push(ch);
        }
    }

    pub(crate) fn backspace_in_picker(&mut self) {
        if let Some(picker) = &mut self.picker {
            picker.backspace();
        }
    }

    /// Switch to the highlighted row and close. A filter that matches nothing
    /// accepts nothing — there is no row under the cursor to mean.
    pub(crate) fn accept_picker(&mut self) {
        let at = self.picker_selected();
        match self.picker.as_ref().map(crate::picker::Picker::kind) {
            Some(crate::picker::PickerKind::Model) => {
                let wanted = self.picker_models().get(at).map(|model| model.id.clone());
                if let Some(wanted) = wanted {
                    self.close_picker();
                    self.set_model_aloud(&wanted);
                }
            }
            Some(crate::picker::PickerKind::Provider) => {
                let wanted = self.picker_providers().get(at).map(|row| row.route.clone());
                if let Some(wanted) = wanted {
                    self.close_picker();
                    self.set_provider_aloud(&wanted);
                }
            }
            // The overlay stays open: signing in to one server is rarely the
            // only thing a person came here to do, and a box that vanishes on
            // enter makes them retype `/mcp` to see whether it worked.
            Some(crate::picker::PickerKind::Mcp) => self.auth_selected_mcp(),
            None => {}
        }
    }

    /// `i` on the overlay, and what enter has always done there: start signing
    /// in to the highlighted server.
    pub(crate) fn auth_selected_mcp(&mut self) {
        let at = self.picker_selected();
        let wanted = self.picker_mcp().get(at).map(|server| server.name.clone());
        if let Some(wanted) = wanted
            && let Err(refusal) = self.mcp_login(&wanted)
        {
            self.mcp_activity.insert(wanted, refusal);
        }
    }

    /// `space` on the overlay: flip whether the highlighted server starts.
    pub(crate) fn toggle_selected_mcp(&mut self) {
        let at = self.picker_selected();
        let rows = self.picker_mcp();
        let Some(server) = rows.get(at) else {
            return;
        };
        let name = server.name.clone();
        let disabled = server.enabled;
        let Some(manage) = self.manage.clone() else {
            self.mcp_activity.insert(
                name,
                "this interface cannot change `.mcp.json` — edit it directly".to_string(),
            );
            return;
        };
        match manage.set_disabled(&name, disabled) {
            Ok(()) => {
                if let Some(row) = self.mcp.iter_mut().find(|row| row.name == name) {
                    row.enabled = !disabled;
                }
                self.mcp_activity.remove(&name);
            }
            Err(refusal) => {
                self.mcp_activity.insert(name, refusal);
            }
        }
    }

    /// `x` on the overlay: drop the highlighted server from whichever file
    /// configured it.
    pub(crate) fn remove_selected_mcp(&mut self) {
        let at = self.picker_selected();
        let rows = self.picker_mcp();
        let Some(server) = rows.get(at) else {
            return;
        };
        let name = server.name.clone();
        let Some(manage) = self.manage.clone() else {
            self.mcp_activity.insert(
                name,
                "this interface cannot change `.mcp.json` — edit it directly".to_string(),
            );
            return;
        };
        match manage.remove(&name) {
            Ok(()) => {
                self.mcp.retain(|row| row.name != name);
                self.mcp_activity.remove(&name);
                if self.mcp.is_empty() {
                    self.close_picker();
                }
            }
            Err(refusal) => {
                self.mcp_activity.insert(name, refusal);
            }
        }
    }

    /// `r` on the overlay: re-read every server from disk.
    pub(crate) fn refresh_mcp(&mut self) {
        let Some(manage) = self.manage.clone() else {
            return;
        };
        match manage.refresh() {
            Ok(servers) => self.mcp = servers,
            Err(refusal) => self.transcript.push(Cell::Error(refusal)),
        }
    }

    pub(crate) fn close_picker(&mut self) {
        self.picker = None;
    }
}
