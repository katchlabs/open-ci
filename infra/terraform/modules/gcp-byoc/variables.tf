variable "project_id" {
  description = "Customer GCP project ID where ephemeral open-ci runners will launch."
  type        = string
}

variable "region" {
  description = "Primary GCP region for runner networking and optional cache storage."
  type        = string
  default     = "us-central1"
}

variable "hosted_control_plane_sa_email" {
  description = "Service account email of the hosted (or remote) open-ci control plane permitted to impersonate the BYOC runner-manager service account."
  type        = string

  validation {
    condition     = can(regex("^[^@\\s]+@[^@\\s]+\\.iam\\.gserviceaccount\\.com$", var.hosted_control_plane_sa_email))
    error_message = "hosted_control_plane_sa_email must be a valid GCP service account email."
  }
}

variable "name_prefix" {
  description = "Prefix for IAM, VPC, firewall, and bucket resources."
  type        = string
  default     = "open-ci"

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{1,20}$", var.name_prefix))
    error_message = "name_prefix must start with a lowercase letter and contain only lowercase letters, digits, and hyphens (2-21 chars)."
  }
}

variable "create_network" {
  description = "Create a dedicated egress-only VPC, subnet, and Cloud NAT for ephemeral runners. Set false to reuse existing_network and existing_subnetwork."
  type        = bool
  default     = true
}

variable "subnet_cidr" {
  description = "CIDR block for the runner subnet when create_network is true."
  type        = string
  default     = "10.44.0.0/20"
}

variable "existing_network" {
  description = "Existing VPC network name or self-link when create_network is false."
  type        = string
  default     = ""
}

variable "existing_subnetwork" {
  description = "Existing subnetwork self-link or resource path (projects/<project>/regions/<region>/subnetworks/<name>) when create_network is false."
  type        = string
  default     = ""
}

variable "create_cache_bucket" {
  description = "Create a regional GCS bucket in the customer project for direct BYOC Actions cache storage."
  type        = bool
  default     = false
}

variable "cache_retention_days" {
  description = "Lifecycle deletion age in days for objects in the optional GCS cache bucket."
  type        = number
  default     = 14
}

variable "labels" {
  description = "Additional GCP labels merged onto created resources."
  type        = map(string)
  default     = {}
}
