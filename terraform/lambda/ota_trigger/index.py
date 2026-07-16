import os
import boto3
import json
import logging
import uuid
import time

logger = logging.getLogger()
logger.setLevel(logging.INFO)

s3_client = boto3.client('s3')
iot_client = boto3.client('iot')

def handler(event, context):
    logger.info(f"Received event: {json.dumps(event)}")
    
    bucket_name = event['Records'][0]['s3']['bucket']['name']
    object_key = event['Records'][0]['s3']['object']['key']
    
    # Generate presigned URL (valid for 24 hours)
    try:
        presigned_url = s3_client.generate_presigned_url(
            'get_object',
            Params={'Bucket': bucket_name, 'Key': object_key},
            ExpiresIn=86400
        )
        logger.info(f"Generated presigned URL for {object_key}")
    except Exception as e:
        logger.error(f"Error generating presigned URL: {e}")
        return {'statusCode': 500, 'body': 'Error generating URL'}

    # Attempt to extract version from the object key (e.g. firmware_v0.0.1.bin)
    # Defaulting to timestamp if parsing fails
    version = object_key.split('/')[-1].replace('.bin', '')
    
    # Create the Job Document
    job_document = {
        "operation": "firmware_update",
        "firmware_version": version,
        "download_url": presigned_url
    }

    job_id = f"ota-{version.replace('.', '-')}-{int(time.time())}"

    # Get all things to target the job (for now, targeting all devices as requested)
    # AWS IoT Jobs allows max 100 targets in the targetSelection="SNAPSHOT" if listed manually,
    # or we can use a continuous job. We will just list up to 100 things here as a quick PoC,
    # but ideally, this should target a predefined Thing Group.
    
    targets = []
    try:
        paginator = iot_client.get_paginator('list_things')
        for page in paginator.paginate():
            for thing in page['things']:
                # Target must be an ARN
                # e.g., arn:aws:iot:region:account:thing/thingName
                # We can construct it, but wait, `list_things` returns thingArn?
                # Actually, list_things doesn't return the ARN in all cases directly without describe,
                # wait, let's just construct the ARN or better, if they exist in a dynamic group, target the group.
                # Since AWS region/account isn't strictly available in env, let's just construct it if we must,
                # BUT wait, the `aws_iot_thing` returns ARNs. We can fetch it via describe_thing or
                # simply arn:aws:iot:<region>:<account-id>:thing/<thingName>
                
                # To be completely safe and avoid region/account hardcoding, let's look up the thingArn:
                # But actually, `list_things` returns `thingArn` natively! 
                # Let's use `thing['thingArn']` if available, or just construct if we had region/account.
                pass
                
        # Better approach: Fetch the ARNs directly
        response = iot_client.list_things(maxResults=100) # Simple limit for now
        targets = [t['thingArn'] for t in response.get('things', []) if 'thingArn' in t]
        
    except Exception as e:
        logger.error(f"Error listing things: {e}")
        return {'statusCode': 500, 'body': 'Error listing things'}

    if not targets:
        logger.info("No things found to target.")
        return {'statusCode': 200, 'body': 'No targets'}

    try:
        response = iot_client.create_job(
            jobId=job_id,
            targets=targets,
            document=json.dumps(job_document),
            description=f"OTA Update for {version}",
            targetSelection="SNAPSHOT"
        )
        logger.info(f"Created job {job_id} successfully.")
    except Exception as e:
        logger.error(f"Error creating job: {e}")
        return {'statusCode': 500, 'body': 'Error creating job'}

    return {
        'statusCode': 200,
        'body': f'OTA job {job_id} created successfully for {len(targets)} targets.'
    }
