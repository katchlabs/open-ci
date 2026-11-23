output "vpc_id" {
  description = "VPC ID created for ephemeral AWS open-ci runners."
  value       = aws_vpc.runners.id
}

output "subnet_ids_by_az" {
  description = "Map of Availability Zone to subnet ID for open-ci placement config."
  value       = { for az, subnet in aws_subnet.runners : az => subnet.id }
}

output "runner_security_group_id" {
  description = "Egress-only security group ID attached to ephemeral AWS runners."
  value       = aws_security_group.runners.id
}

output "controller_policy_arn" {
  description = "ARN of the least-privilege IAM policy for the open-ci controller."
  value       = aws_iam_policy.controller.arn
}

output "janitor_lambda_arn" {
  description = "ARN of the deployed Rust AWS expiry janitor Lambda (empty if janitor_zip_path unset)."
  value       = length(aws_lambda_function.janitor) > 0 ? aws_lambda_function.janitor[0].arn : ""
}

output "gcp_controller_role_arn" {
  description = "IAM role ARN for aws.role_arn on a GCP-hosted controller (empty unless federate_gcp_controller is true)."
  value       = length(aws_iam_role.gcp_controller) > 0 ? aws_iam_role.gcp_controller[0].arn : ""
}

output "web_identity_audience" {
  description = "Value for aws.web_identity_audience in open-ci.toml (empty unless federate_gcp_controller is true)."
  value       = var.federate_gcp_controller ? var.web_identity_audience : ""
}
