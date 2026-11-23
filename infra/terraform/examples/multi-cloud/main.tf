terraform {
  required_version = ">= 1.6.0"

  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 6.0"
    }
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

variable "gcp_project_id" {
  description = "GCP project ID for the controller and primary runner pools."
  type        = string
}

variable "gcp_region" {
  type    = string
  default = "us-central1"
}

variable "gcp_zone" {
  type    = string
  default = "us-central1-a"
}

variable "aws_region" {
  description = "AWS region for secondary/burst runner pools."
  type        = string
  default     = "us-east-1"
}

variable "domain" {
  description = "Public HTTPS domain for the open-ci control plane."
  type        = string
}

variable "janitor_zip_path" {
  description = "Optional path to compiled open-ci-aws-janitor Lambda zip."
  type        = string
  default     = ""
}

provider "google" {
  project = var.gcp_project_id
  region  = var.gcp_region
}

provider "aws" {
  region = var.aws_region
}

module "gcp_foundation" {
  source                           = "../../modules/gcp-foundation"
  project_id                       = var.gcp_project_id
  region                           = var.gcp_region
  controller_service_account_email = module.gcp_controller.controller_service_account_email
}

module "gcp_controller" {
  source            = "../../modules/gcp-controller"
  project_id        = var.gcp_project_id
  region            = var.gcp_region
  zone              = var.gcp_zone
  domain            = var.domain
  network_self_link = module.gcp_foundation.network_self_link
  subnet_self_link  = module.gcp_foundation.subnet_self_link
}

module "aws_foundation" {
  source           = "../../modules/aws-foundation"
  aws_region       = var.aws_region
  janitor_zip_path = var.janitor_zip_path
}

output "controller_ip" {
  value = module.gcp_controller.controller_ip
}

output "gcp_subnet" {
  value = module.gcp_foundation.subnet_self_link
}

output "aws_subnets" {
  value = module.aws_foundation.subnet_ids_by_az
}

output "aws_security_group" {
  value = module.aws_foundation.runner_security_group_id
}

output "aws_controller_policy_arn" {
  value = module.aws_foundation.controller_policy_arn
}
