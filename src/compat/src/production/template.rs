use std::collections::BTreeMap;

use a3s_box_core::resolve_execution;
use async_trait::async_trait;

use crate::control::{
    ResolvedTemplate, TemplateProvider, TemplateProviderError, TemplateProviderResult,
};

/// Immutable startup-validated template catalog.
#[derive(Debug, Clone)]
pub struct StaticTemplateProvider {
    templates: BTreeMap<String, ResolvedTemplate>,
}

impl StaticTemplateProvider {
    pub fn new(
        templates: impl IntoIterator<Item = (String, ResolvedTemplate)>,
    ) -> TemplateProviderResult<Self> {
        let mut catalog = BTreeMap::new();
        for (template_id, template) in templates {
            validate_template(&template_id, &template)?;
            if catalog.insert(template_id.clone(), template).is_some() {
                return Err(TemplateProviderError::Invalid(format!(
                    "template ID {template_id} is configured more than once"
                )));
            }
        }
        if catalog.is_empty() {
            return Err(TemplateProviderError::Invalid(
                "at least one template policy is required".to_string(),
            ));
        }
        Ok(Self { templates: catalog })
    }

    pub fn contains(&self, template_id: &str) -> bool {
        self.templates.contains_key(template_id)
    }

    pub fn len(&self) -> usize {
        self.templates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }
}

#[async_trait]
impl TemplateProvider for StaticTemplateProvider {
    async fn resolve(
        &self,
        _owner_id: &str,
        template_id: &str,
    ) -> TemplateProviderResult<ResolvedTemplate> {
        self.templates
            .get(template_id)
            .cloned()
            .ok_or_else(|| TemplateProviderError::NotFound(template_id.to_string()))
    }
}

fn validate_template(template_id: &str, template: &ResolvedTemplate) -> TemplateProviderResult<()> {
    if template_id.is_empty()
        || template_id.len() > 128
        || template_id.chars().any(char::is_whitespace)
    {
        return Err(TemplateProviderError::Invalid(format!(
            "template ID {template_id:?} is invalid"
        )));
    }
    if template.config.image.trim().is_empty() {
        return Err(TemplateProviderError::Invalid(format!(
            "template {template_id} has no OCI image"
        )));
    }
    if template.envd_version.trim().is_empty() {
        return Err(TemplateProviderError::Invalid(format!(
            "template {template_id} has no envd version"
        )));
    }
    template.routing.validate().map_err(|error| {
        TemplateProviderError::Invalid(format!(
            "template {template_id} has an invalid route policy: {error}"
        ))
    })?;
    resolve_execution(&template.config).map_err(|error| {
        TemplateProviderError::Invalid(format!(
            "template {template_id} has an invalid execution policy: {error}"
        ))
    })?;
    Ok(())
}
