variable "name_prefix" {
  description = "Prefix applied to AWS VPC, IAM, and janitor resources."
  type        = string
  default     = "open-ci"
}

variable "aws_region" {
  description = "AWS region where open-ci runner infrastructure is provisioned."
  type        = string
  default     = "us-east-1"
}

variable "vpc_cidr" {
  description = "IPv4 CIDR block for the isolated open-ci runner VPC."
  type        = string
  default     = "10.44.0.0/16"
}

variable "availability_zones" {
  description = "Availability zones in which to create runner subnets."
  type        = list(string)
  default     = ["us-east-1a", "us-east-1b"]
}

variable "janitor_zip_path" {
  description = "Optional path to the compiled open-ci-aws-janitor Lambda zip package."
  type        = string
  default     = ""
}

variable "create_spot_service_linked_role" {
  description = "Create the account-wide AWSServiceRoleForEC2Spot role required for API Spot launches. Set false when the account already has it."
  type        = bool
  default     = true
}

variable "janitor_max_lifetime_seconds" {
  description = "Absolute runner lifetime ceiling enforced by the janitor from launch time. Must be at least the largest pools.*.lifetime_seconds."
  type        = number
  default     = 3600

  validation {
    condition     = var.janitor_max_lifetime_seconds >= 120 && var.janitor_max_lifetime_seconds <= 21600
    error_message = "janitor_max_lifetime_seconds must be between 120 and 21600 (the pool lifetime_seconds bounds)."
  }
}

variable "janitor_dry_run" {
  description = "When true, the expiry janitor logs expired resources without terminating them."
  type        = bool
  default     = false
}

variable "alarm_sns_topic_arn" {
  description = "Optional SNS topic ARN notified when the expiry janitor Lambda errors."
  type        = string
  default     = ""
}

variable "tags" {
  description = "Additional AWS tags applied to all provisioned resources."
  type        = map(string)
  default     = {}
}

variable "federate_gcp_controller" {
  description = "Create an IAM role that a GCP-hosted controller assumes keylessly via AssumeRoleWithWebIdentity."
  type        = bool
  default     = false
}

variable "gcp_controller_service_account_unique_id" {
  description = "Numeric unique ID of the GCP controller service account (required when federate_gcp_controller is true)."
  type        = string
  default     = ""
}

variable "web_identity_audience" {
  description = "Audience the GCP controller requests on its identity token; must equal aws.web_identity_audience in open-ci.toml."
  type        = string
  default     = "open-ci-aws-federation"

  validation {
    condition     = can(regex("^[!-~]{1,200}$", var.web_identity_audience))
    error_message = "web_identity_audience must be 1-200 non-whitespace printable ASCII characters (matching open-ci config validation)."
  }
}
