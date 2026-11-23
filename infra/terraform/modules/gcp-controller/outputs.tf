output "controller_ip" {
  description = "Static external IPv4 address for DNS A record pointing to open-ci."
  value       = google_compute_address.controller.address
}

output "controller_service_account_email" {
  description = "Service account email assigned to the open-ci controller VM."
  value       = google_service_account.controller.email
}

output "secret_ids" {
  description = "Secret Manager secret IDs to populate with open-ci runtime secrets."
  value       = { for k, v in google_secret_manager_secret.controller : k => v.secret_id }
}

output "public_url" {
  description = "Base HTTPS URL for the open-ci control plane."
  value       = "https://${var.domain}"
}
