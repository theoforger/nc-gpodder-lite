use crate::json::PhpJson;

/// Nextcloud's `status.php`, which some clients probe before logging in.
pub async fn status() -> PhpJson {
    PhpJson::ok(serde_json::json!({
        "installed": true,
        "maintenance": false,
        "needsDbUpgrade": false,
        "version": "32.0.0.0",
        "versionstring": "32.0.0",
        "edition": "",
        "productname": "Nextcloud",
        "extendedSupport": false,
    }))
}
