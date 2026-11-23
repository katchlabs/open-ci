terraform {
  required_version = ">= 1.6.0"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

variable "aws_region" {
  description = "AWS region for the open-ci control plane and runner VPC."
  type        = string
  default     = "us-east-1"
}

variable "availability_zones" {
  description = "Availability zones for runner subnets (the first AZ also hosts the controller)."
  type        = list(string)
  default     = ["us-east-1a", "us-east-1b"]
}

variable "domain" {
  description = "Public HTTPS domain for the open-ci control plane (e.g. ci.example.com)."
  type        = string
}

variable "controller_architecture" {
  description = "CPU architecture for the controller host (arm64 or x86_64)."
  type        = string
  default     = "arm64"
}

variable "controller_instance_type" {
  description = "EC2 instance type for the controller host."
  type        = string
  default     = "t4g.small"
}

variable "assume_role_arns" {
  description = "Optional cross-account IAM role ARNs for BYOC runner provisioning."
  type        = list(string)
  default     = []
}

variable "backup_bucket_arn" {
  description = "Optional S3 bucket ARN for encrypted SQLite state backups."
  type        = string
  default     = ""
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

provider "aws" {
  region = var.aws_region
}

module "aws_foundation" {
  source             = "../../modules/aws-foundation"
  aws_region         = var.aws_region
  availability_zones = var.availability_zones
  janitor_zip_path   = var.janitor_zip_path

  janitor_max_lifetime_seconds = var.janitor_max_lifetime_seconds

  create_spot_service_linked_role = var.create_spot_service_linked_role
}

module "aws_controller" {
  source                = "../../modules/aws-controller"
  vpc_id                = module.aws_foundation.vpc_id
  subnet_id             = module.aws_foundation.subnet_ids_by_az[var.availability_zones[0]]
  domain                = var.domain
  architecture          = var.controller_architecture
  instance_type         = var.controller_instance_type
  controller_policy_arn = module.aws_foundation.controller_policy_arn
  assume_role_arns      = var.assume_role_arns
  backup_bucket_arn     = var.backup_bucket_arn
}

output "controller_ip" {
  value = module.aws_controller.controller_ip
}

output "public_url" {
  value = module.aws_controller.public_url
}

output "controller_iam_role_arn" {
  value = module.aws_controller.controller_iam_role_arn
}

output "runner_subnets" {
  value = module.aws_foundation.subnet_ids_by_az
}

output "runner_security_group_id" {
  value = module.aws_foundation.runner_security_group_id
}

output "ssm_session_command" {
  value = module.aws_controller.ssm_session_command
}
