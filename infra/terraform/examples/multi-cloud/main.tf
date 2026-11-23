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

variable "aws_availability_zones" {
  description = "AWS Availability Zones for runner subnets in aws_region."
  type        = list(string)
  default     = ["us-east-1a", "us-east-1b"]
}

variable "domain" {
  description = "Public HTTPS domain for the open-ci control plane."
  type        = string
}

variable "create_spot_service_linked_role" {
  description = "Create the account-wide EC2 Spot service-linked role. Set false if the account already has AWSServiceRoleForEC2Spot."
  type        = bool
  default     = true
}

variable "janitor_max_lifetime_seconds" {
  description = "Janitor runner lifetime ceiling from launch; must be at least the largest pools.*.lifetime_seconds."
  type        = number
  default     = 3600
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
  source             = "../../modules/aws-foundation"
  aws_region         = var.aws_region
  availability_zones = var.aws_availability_zones
  janitor_zip_path   = var.janitor_zip_path

  janitor_max_lifetime_seconds = var.janitor_max_lifetime_seconds

  create_spot_service_linked_role          = var.create_spot_service_linked_role
  federate_gcp_controller                  = true
  gcp_controller_service_account_unique_id = module.gcp_controller.controller_service_account_unique_id
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

output "aws_role_arn" {
  description = "Set as aws.role_arn in open-ci.toml; the GCP controller assumes it keylessly."
  value       = module.aws_foundation.gcp_controller_role_arn
}

output "aws_web_identity_audience" {
  description = "Set as aws.web_identity_audience in open-ci.toml."
  value       = module.aws_foundation.web_identity_audience
}
