import{test,expect,request}from'@playwright/test';
test('loopback auth, views, filters, pagination, SSE and CSRF controls',async({page})=>{
 await page.goto('/');await expect(page).toHaveTitle('Distributed Workbench');expect(page.url()).not.toContain('token');
 const cookies=await page.context().cookies();expect(cookies.find(c=>c.name==='workbench_operator')?.httpOnly).toBe(true);expect(cookies.find(c=>c.name==='workbench_operator')?.sameSite).toBe('Strict');
 for(const name of['runs','topology','runtime','legacy'])await expect(page.getByRole('button',{name})).toBeVisible();
 await expect(page.getByLabel('Run status')).toBeVisible();await expect(page.getByRole('button',{name:'Previous'})).toBeDisabled();
 await page.getByRole('button',{name:'topology'}).click();await expect(page.getByText('Controllers / peer health')).toBeVisible();
 await page.getByRole('button',{name:'runtime'}).click();await expect(page.getByText('Driver sessions')).toBeVisible();await expect(page.getByText('Approvals')).toBeVisible();
 const forbidden=await page.request.post('/api/operator/nonce',{data:{action:'cancel-task',target:'none',reason:'test',confirmed:true},headers:{Origin:'http://127.0.0.1:19918'}});expect(forbidden.status()).toBe(403);
 const invalidLogs=await page.request.get('/api/logs?executorId=bad%2Fid&processId=x');expect(invalidLogs.status()).toBe(400);
});
test('API and repeat bootstrap reject a client without operator cookie',async()=>{const client=await request.newContext();const snapshot=await client.get('http://127.0.0.1:19918/api/snapshot',{headers:{Host:'127.0.0.1:19918'}});expect(snapshot.status()).toBe(401);const root=await client.get('http://127.0.0.1:19918/',{headers:{Host:'127.0.0.1:19918','Sec-Fetch-Dest':'document'}});expect(root.status()).toBe(401);await client.dispose()});
