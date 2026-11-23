variable "name_prefix" {
  description = "Prefix applied to controller resources."
  type        = string
  default     = "open-ci"
}

variable "vpc_id" {
  description = "VPC ID where the open-ci controller EC2 instance runs."
  type        = string
}

variable "subnet_id" {
  description = "Public subnet ID where the open-ci controller EC2 instance runs."
  type        = string
}

variable "domain" {
  description = "Public HTTPS domain routed to this open-ci controller (used by Caddy TLS and public_url)."
  type        = string
}

variable "architecture" {
  description = "CPU architecture for the controller host (arm64 or x86_64)."
  type        = string
  default     = "arm64"

  validation {
    condition     = contains(["arm64", "x86_64"], var.architecture)
    error_message = "architecture must be either \"arm64\" or \"x86_64\"."
  }
}

variable "instance_type" {
  description = "EC2 instance type for the controller host (matches architecture, e.g. t4g.small for arm64 or t3.small for x86_64)."
  type        = string
  default     = "t4g.small"
}

variable "ami_id" {
  description = "Optional custom AMI ID for the controller host. When empty, uses the latest official Debian 12 AMI for var.architecture."
  type        = string
  default     = ""
}

variable "root_volume_size_gb" {
  description = "Size of the encrypted gp3 root EBS volume in GiB."
  type        = number
  default     = 25
}

variable "ingress_cidrs" {
  description = "CIDR blocks permitted to reach HTTPS (443) and HTTP (80 for ACME) on the controller. Port 8080 remains private on loopback."
  type        = list(string)
  default     = ["0.0.0.0/0"]
}

variable "controller_policy_arn" {
  description = "Optional pre-existing IAM policy ARN (e.g. from aws-foundation) granting EC2 runner lifecycle permissions. When empty, a scoped policy is created on the role."
  type        = string
  default     = ""
}

variable "assume_role_arns" {
  description = "Optional list of cross-account IAM role ARNs this controller is allowed to assume via STS AssumeRole."
  type        = list(string)
  default     = []
}

variable "backup_bucket_arn" {
  description = "Optional S3 bucket ARN for encrypted SQLite backups (grants least-privilege PutObject/GetObject/ListBucket)."
  type        = string
  default     = ""
}

variable "tags" {
  description = "Additional AWS tags applied to controller resources."
  type        = map(string)
  default     = {}
}
