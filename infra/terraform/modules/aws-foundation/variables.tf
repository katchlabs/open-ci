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
