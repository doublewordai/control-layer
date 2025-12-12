#!/usr/bin/env python3
"""
Test script to verify if a StandardUser can gift credits to another user
on production server using different auth methods.
"""

import requests
import sys
from typing import Optional, Tuple

BASE_URL = "https://app.doubleword.ai"

# Platform Manager API Key (for getting target user info)
PLATFORM_MANAGER_API_KEY = "sk-aS-WW6tXLWlaaS4Ge6U9DEoakHGe1NaAMEvnGrVCRZU"


def get_user_info_with_api_key(api_key: str) -> Optional[dict]:
    """Get current user info using API key."""
    print(f"\n📋 Fetching user info with API key...")
    
    response = requests.get(
        f"{BASE_URL}/admin/api/v1/users/current",
        headers={"Authorization": f"Bearer {api_key}"}
    )
    
    if response.status_code == 200:
        user_data = response.json()
        print(f"✅ User ID: {user_data.get('id')}")
        print(f"   Email: {user_data.get('email')}")
        print(f"   Roles: {user_data.get('roles')}")
        print(f"   Is Admin: {user_data.get('is_admin')}")
        return user_data
    else:
        print(f"❌ Failed to get user info: {response.status_code}")
        print(f"Response: {response.text}")
        return None


def get_user_info_with_session(session_cookie: str) -> Optional[dict]:
    """Get current user info using session cookie."""
    print(f"\n📋 Fetching user info with session cookie...")
    
    response = requests.get(
        f"{BASE_URL}/admin/api/v1/users/current",
        cookies={"dw_cookie": session_cookie}  # Changed from dwctl_session
    )
    
    if response.status_code == 200:
        user_data = response.json()
        print(f"✅ User ID: {user_data.get('id')}")
        print(f"   Email: {user_data.get('email')}")
        print(f"   Roles: {user_data.get('roles')}")
        print(f"   Is Admin: {user_data.get('is_admin')}")
        return user_data
    else:
        print(f"❌ Failed to get user info: {response.status_code}")
        print(f"Response: {response.text[:500] if response.text else 'No response body'}")
        return None


def list_all_users_with_api_key(api_key: str) -> Optional[list]:
    """List all users using platform manager API key."""
    print(f"\n👥 Listing all users (to find standard user)...")
    
    response = requests.get(
        f"{BASE_URL}/admin/api/v1/users",
        headers={"Authorization": f"Bearer {api_key}"}
    )
    
    if response.status_code == 200:
        users_data = response.json()
        users = users_data.get('data', [])
        print(f"✅ Found {len(users)} users")
        
        # Show StandardUsers
        standard_users = [u for u in users if 'StandardUser' in u.get('roles', []) and not u.get('is_admin', False)]
        print(f"\n   Standard Users:")
        for user in standard_users[:5]:  # Show first 5
            print(f"   - {user.get('email')} (ID: {user.get('id')})")
        
        return users
    else:
        print(f"❌ Failed to list users: {response.status_code}")
        print(f"Response: {response.text}")
        return None


def create_payment_with_session(
    session_cookie: str,
    creditee_id: str
) -> Optional[dict]:
    """
    Attempt to create a payment session that credits another user using session cookie.
    This should ONLY work for BillingManager/PlatformManager roles.
    """
    print(f"\n💳 Attempting to create payment for user {creditee_id}...")
    print(f"   Using session cookie authentication")
    
    response = requests.post(
        f"{BASE_URL}/admin/api/v1/payments",
        params={"creditee_id": creditee_id},
        cookies={"dw_cookie": session_cookie}  # Changed from dwctl_session
    )
    
    print(f"   Status Code: {response.status_code}")
    
    if response.status_code == 200:
        data = response.json()
        print(f"🚨 Payment session created!")
        print(f"   Checkout URL: {data.get('url')}")
        return data
    elif response.status_code == 403:
        try:
            error_data = response.json()
            print(f"✅ BLOCKED - Permission denied (as expected)")
            print(f"   Response: {error_data}")
        except:
            print(f"✅ BLOCKED - Permission denied (as expected)")
            print(f"   Response: {response.text[:200]}")
        return None
    else:
        print(f"⚠️  Unexpected response: {response.status_code}")
        print(f"   Response: {response.text[:500] if response.text else 'No response body'}")
        return None


def main():
    print("=" * 80)
    print("🔍 PRODUCTION SECURITY TEST: Can StandardUser gift credits?")
    print(f"🌐 Server: {BASE_URL}")
    print("=" * 80)
    
    # Step 1: Get platform manager info and list users
    print("\n" + "=" * 80)
    print("STEP 1: Get target user using Platform Manager API key")
    print("=" * 80)
    
    platform_manager = get_user_info_with_api_key(PLATFORM_MANAGER_API_KEY)
    if not platform_manager:
        print("\n❌ Failed to authenticate platform manager")
        sys.exit(1)
    
    platform_manager_id = platform_manager.get('id')
    
    # List users to find a standard user
    all_users = list_all_users_with_api_key(PLATFORM_MANAGER_API_KEY)
    if not all_users:
        print("\n❌ Failed to list users")
        sys.exit(1)
    
    # Find first non-admin StandardUser as target
    target_user = None
    for user in all_users:
        if 'StandardUser' in user.get('roles', []) and not user.get('is_admin', False):
            target_user = user
            break
    
    if not target_user:
        print("\n❌ No StandardUser found to use as target")
        sys.exit(1)
    
    target_user_id = target_user.get('id')
    print(f"\n✅ Will attempt to gift credits to: {target_user.get('email')} ({target_user_id})")
    
    # Step 2: Get session cookie from SSO user
    print("\n" + "=" * 80)
    print("STEP 2: Get StandardUser session cookie")
    print("=" * 80)
    print("\n⚠️  MANUAL STEP REQUIRED:")
    print("   1. Open browser and login to https://app.doubleword.ai as StandardUser")
    print("   2. Open DevTools (F12) → Application/Storage → Cookies")
    print("   3. Find cookie named 'dw_cookie'")  # Changed from dwctl_session
    print("   4. Copy the entire cookie value")
    print()
    
    session_cookie = input("📋 Paste the dw_cookie value here: ").strip()  # Changed prompt
    
    if not session_cookie:
        print("\n❌ No session cookie provided")
        sys.exit(1)
    
    # Verify the session cookie works and user is StandardUser
    print("\n" + "=" * 80)
    print("STEP 3: Verify StandardUser session")
    print("=" * 80)
    
    standard_user = get_user_info_with_session(session_cookie)
    if not standard_user:
        print("\n❌ Failed to authenticate with session cookie")
        print("   Make sure you copied the full cookie value")
        sys.exit(1)
    
    # Check if user is actually a StandardUser
    roles = standard_user.get('roles', [])
    is_admin = standard_user.get('is_admin', False)
    
    if is_admin or 'PlatformManager' in roles or 'BillingManager' in roles:
        print("\n⚠️  WARNING: This user has admin/manager privileges!")
        print(f"   Roles: {roles}")
        print("   This test needs a StandardUser without elevated permissions")
        
        proceed = input("\nContinue anyway? (y/N): ").strip().lower()
        if proceed != 'y':
            sys.exit(1)
    
    # Step 4: Attempt the attack
    print("\n" + "=" * 80)
    print("STEP 4: TEST - StandardUser attempts to gift credits")
    print("=" * 80)
    print(f"\n🎯 StandardUser: {standard_user.get('email')}")
    print(f"🎁 Target (gift recipient): {target_user.get('email')}")
    print(f"📝 Will call: POST /payments?creditee_id={target_user_id}")
    
    payment_data = create_payment_with_session(session_cookie, target_user_id)
    
    # Analyze results
    print("\n" + "=" * 80)
    print("📊 RESULTS")
    print("=" * 80)
    
    if payment_data:
        print("❌ SECURITY VULNERABILITY CONFIRMED!")
        print("   StandardUser CAN create payments for other users")
        print("   This allows credit gifting without proper authorization")
        print("\n   Expected: 403 Forbidden")
        print("   Actual: 200 OK with checkout URL")
        
        # Check if the checkout URL contains the target user ID
        checkout_url = payment_data.get("url", "")
        if target_user_id in checkout_url:
            print(f"\n   ⚠️  Checkout URL contains target user ID: {target_user_id}")
            print("   This confirms credits would go to the target, not the payer")
        
        print("\n🔧 RECOMMENDED FIX:")
        print("   Add permission check in create_payment handler:")
        print("   - Require Credits::CreateAll for creditee_id usage")
        print("   - Only BillingManager/PlatformManager should be allowed")
        
        return 1  # Exit with error code
    else:
        print("✅ SECURITY CHECK PASSED!")
        print("   StandardUser properly blocked from crediting other users")
        return 0


if __name__ == "__main__":
    try:
        exit_code = main()
        sys.exit(exit_code)
    except KeyboardInterrupt:
        print("\n\n⚠️  Test interrupted by user")
        sys.exit(130)
    except Exception as e:
        print(f"\n\n❌ Unexpected error: {e}")
        import traceback
        traceback.print_exc()
        sys.exit(1)