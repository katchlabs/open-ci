variable "project_id" {
  description = "GCP project ID where the open-ci controller VM runs."
  type        = string
}

variable "region" {
  description = "GCP region for the controller static IP and subnetwork."
  type        = string
  default     = "us-central1"
}

variable "zone" {
  description = "GCP zone for the controller Compute Engine VM."
  type        = string
  default     = "us-central1-a"
}

variable "name_prefix" {
  description = "Prefix applied to controller resources."
  type        = string
  default     = "open-ci"
}

variable "network_self_link" {
  description = "VPC self-link from the gcp-foundation module."
  type        = string
}

variable "subnet_self_link" {
  description = "Regional subnet self-link from the gcp-foundation module."
  type        = string
}

variable "domain" {
  description = "Public HTTPS domain routed to this open-ci controller (used by Caddy TLS and public_url)."
  type        = string
}

variable "machine_type" {
  description = "Compute Engine machine type for the controller host."
  type        = string
  default     = "e2-small"
}

variable "boot_image" {
  description = "Base Linux image for the controller host."
  type        = string
  default     = "debian-cloud/debian-12"
}

variable "labels" {
  description = "Additional GCP labels applied to controller resources."
  type        = map(string)
  default     = {}
}
