output "service_account_email" {
  description = "Customer BYOC runner-manager service account email impersonated by open-ci."
  value       = google_service_account.runner_manager.email
}

output "network" {
  description = "VPC network ID used by ephemeral runners."
  value       = local.network_name
}

output "subnetwork" {
  description = "Subnetwork resource path formatted for open-ci placement configuration."
  value       = local.subnetwork_path
}

output "network_tag" {
  description = "Network tag enforcing deny-all-ingress on ephemeral runners."
  value       = local.runner_network_tag
}

output "cache_bucket_name" {
  description = "Optional regional GCS cache bucket name (empty if create_cache_bucket is false)."
  value       = var.create_cache_bucket ? google_storage_bucket.cache[0].name : ""
}

output "connection_bundle" {
  description = "Structured BYOC connection bundle ready to paste into the Hosted open-ci onboarding wizard."
  value = {
    provider              = "gcp"
    project_id            = var.project_id
    region                = var.region
    service_account_email = google_service_account.runner_manager.email
    network               = local.network_name
    subnetwork            = local.subnetwork_path
    network_tag           = local.runner_network_tag
    cache_bucket          = var.create_cache_bucket ? google_storage_bucket.cache[0].name : null
  }
}
