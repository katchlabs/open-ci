output "role_arn" {
  description = "Customer BYOC IAM Role ARN assumed by open-ci via sts:AssumeRole with ExternalId."
  value       = aws_iam_role.byoc.arn
}

output "account_id" {
  description = "Customer AWS account ID."
  value       = data.aws_caller_identity.current.account_id
}

output "vpc_id" {
  description = "VPC ID used by ephemeral runners."
  value       = local.vpc_id
}

output "subnet_ids" {
  description = "Subnet IDs permitted for ephemeral runner launches."
  value       = local.subnet_ids
}

output "security_group_id" {
  description = "Egress-only security group ID attached to ephemeral runners."
  value       = aws_security_group.runners.id
}

output "cache_bucket_name" {
  description = "Optional regional S3 cache bucket name (empty if create_cache_bucket is false)."
  value       = var.create_cache_bucket ? aws_s3_bucket.cache[0].bucket : ""
}

output "connection_bundle" {
  description = "Structured BYOC connection bundle ready to paste into the Hosted open-ci onboarding wizard."
  value = {
    provider          = "aws"
    account_id        = data.aws_caller_identity.current.account_id
    region            = var.aws_region
    role_arn          = aws_iam_role.byoc.arn
    external_id       = var.external_id
    security_group_id = aws_security_group.runners.id
    subnet_ids        = local.subnet_ids
    cache_bucket      = var.create_cache_bucket ? aws_s3_bucket.cache[0].bucket : null
  }
}
