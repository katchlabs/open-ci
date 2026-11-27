variable "aws_region" {
  description = "AWS region where ephemeral open-ci runners will launch."
  type        = string
  default     = "us-east-1"
}

variable "trusted_principal_arn" {
  description = "IAM Role ARN of the hosted (or remote) open-ci AWS Hub Role permitted to assume the BYOC role."
  type        = string

  validation {
    condition     = can(regex("^arn:aws(-[a-z]+)?:iam::[0-9]{12}:role/.+$", var.trusted_principal_arn))
    error_message = "trusted_principal_arn must be a valid AWS IAM Role ARN."
  }
}

variable "external_id" {
  description = "Unique high-entropy ExternalId issued by open-ci to prevent confused-deputy attacks."
  type        = string

  validation {
    condition     = length(var.external_id) >= 16
    error_message = "external_id must be at least 16 characters."
  }
}

variable "name_prefix" {
  description = "Prefix for IAM, VPC, security group, and optional cache bucket resources."
  type        = string
  default     = "open-ci"

  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{1,20}$", var.name_prefix))
    error_message = "name_prefix must start with a lowercase letter and contain only lowercase letters, digits, and hyphens (2-21 chars)."
  }
}

variable "create_vpc" {
  description = "Create a dedicated runner VPC, IGW, and public subnets. Set false to reuse existing_vpc_id and existing_subnet_ids."
  type        = bool
  default     = true
}

variable "vpc_cidr" {
  description = "CIDR block for the dedicated runner VPC when create_vpc is true."
  type        = string
  default     = "10.45.0.0/16"
}

variable "availability_zones" {
  description = "Availability zones for dedicated runner subnets when create_vpc is true."
  type        = list(string)
  default     = ["us-east-1a", "us-east-1b"]
}

variable "existing_vpc_id" {
  description = "Existing VPC ID when create_vpc is false."
  type        = string
  default     = ""
}

variable "existing_subnet_ids" {
  description = "Existing subnet IDs when create_vpc is false."
  type        = list(string)
  default     = []
}

variable "create_spot_service_linked_role" {
  description = "Create AWSServiceRoleForEC2Spot if this AWS account has never launched EC2 Spot instances."
  type        = bool
  default     = false
}

variable "create_cache_bucket" {
  description = "Create a private regional S3 bucket in the customer account for direct BYOC Actions caching."
  type        = bool
  default     = false
}

variable "cache_retention_days" {
  description = "Lifecycle deletion age in days for objects in the optional S3 cache bucket."
  type        = number
  default     = 14
}

variable "tags" {
  description = "Additional AWS tags merged onto created resources."
  type        = map(string)
  default     = {}
}
