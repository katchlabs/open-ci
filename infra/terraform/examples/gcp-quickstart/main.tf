terraform {
  required_version = ">= 1.6.0"

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 6.0"
    }
  }
}

variable "project_id" {
  description = "GCP project ID for open-ci."
  type        = string
}

variable "region" {
  description = "Primary GCP region."
  type        = string
  default     = "us-central1"
}

variable "zone" {
  description = "GCP zone for the controller VM."
  type        = string
  default     = "us-central1-a"
}

variable "domain" {
  description = "Public HTTPS domain for the open-ci control plane (e.g. ci.example.com)."
  type        = string
}

provider "google" {
  project = var.project_id
  region  = var.region
}

module "foundation" {
  source                           = "../../modules/gcp-foundation"
  project_id                       = var.project_id
  region                           = var.region
  controller_service_account_email = module.controller.controller_service_account_email
}

module "controller" {
  source            = "../../modules/gcp-controller"
  project_id        = var.project_id
  region            = var.region
  zone              = var.zone
  domain            = var.domain
  network_self_link = module.foundation.network_self_link
  subnet_self_link  = module.foundation.subnet_self_link
}

output "controller_ip" {
  value = module.controller.controller_ip
}

output "public_url" {
  value = module.controller.public_url
}

output "backup_bucket" {
  value = module.foundation.backup_bucket_name
}

output "secret_ids" {
  value = module.controller.secret_ids
}
