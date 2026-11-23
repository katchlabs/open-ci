variable "project_id" {
  description = "GCP project ID where open-ci runner infrastructure is provisioned."
  type        = string
}

variable "region" {
  description = "Primary GCP region for the runner VPC, subnet, and Cloud NAT."
  type        = string
  default     = "us-central1"
}

variable "name_prefix" {
  description = "Prefix applied to all provisioned network, IAM, and storage resources."
  type        = string
  default     = "open-ci"
}

variable "subnet_cidr" {
  description = "Primary IPv4 CIDR block for the private runner subnet."
  type        = string
  default     = "10.42.0.0/20"
}

variable "controller_service_account_email" {
  description = "Optional controller service account email to grant runner management and backup permissions."
  type        = string
  default     = ""
}

variable "backup_retention_days" {
  description = "Number of days to retain encrypted SQLite state backups in GCS."
  type        = number
  default     = 30

  validation {
    condition     = var.backup_retention_days >= 1 && var.backup_retention_days <= 365
    error_message = "backup_retention_days must be between 1 and 365."
  }
}

variable "labels" {
  description = "Additional GCP labels applied to supported resources."
  type        = map(string)
  default     = {}
}
