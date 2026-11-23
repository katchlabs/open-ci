output "network_self_link" {
  description = "Self-link of the isolated open-ci runner VPC."
  value       = google_compute_network.runners.self_link
}

output "subnet_self_link" {
  description = "Self-link of the regional private runner subnet."
  value       = google_compute_subnetwork.runners.self_link
}

output "custom_role_name" {
  description = "Fully qualified resource name of the least-privilege open-ci IAM role."
  value       = google_project_iam_custom_role.runner_controller.name
}

output "backup_bucket_name" {
  description = "GCS bucket name for open-ci online SQLite state backups."
  value       = google_storage_bucket.backups.name
}
