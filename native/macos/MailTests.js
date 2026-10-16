// 仅假 Mail 对象；绝不发送真实邮件。
function selfTestMail() {
    let passed = 0;
    function assert(value, message) { if (!value) throw new Error(message); passed++; }
    function rejects(fn, code) { let found = false; try { fn(); } catch (e) { found = e.code === code; } assert(found, 'expected ' + code); }
    ['张经理', 'A <a@example.com>', 'a@example.com\nBcc:x@y.com'].forEach(x => rejects(() => email(x), 'bad_args'));
    rejects(() => addresses({to:['a@example.com'],cc:['A@example.com']}), 'bad_args');
    rejects(() => requestID('../unsafe'), 'bad_args');
    rejects(() => numericID('9007199254740999'), 'bad_args');
    const records = [
        {id:'3',message_id:'c',headers:'Message-ID: <c@x>\nIn-Reply-To: <b@x>'},
        {id:'2',message_id:'b',headers:'Message-ID: <b@x>\nReferences: <a@x>'},
        {id:'1',message_id:'a',headers:'Message-ID: <a@x>'},
        {id:'4',message_id:'d',headers:'Subject: 相同主题\nMessage-ID: <unrelated@x>'},
        {id:'5',message_id:'a',headers:'Message-ID: <a@x>'}
    ];
    assert(threadMembers(records, records[2]).length === 3, 'transitive thread and dedup');
    rejects(() => threadMembers(records,{headers:'Subject: 相同主题'}), 'unsupported');
    assert(referenceIDs('References: <a@x>\n\t<b@x>').size === 2, 'folded headers');
    const paged = page({body:'云'.repeat(8001),headers:'private'},0);
    assert(paged.body.length === 8000 && paged.next_offset === 8000 && !paged.headers, 'body pagination');
    rejects(() => page({body:'x'},2), 'bad_args');
    assert(collection(null).length===0, 'Mail missing-value is an empty collection');
    rejects(()=>collection({}), 'invalid_response');
    function searchFixture(count, fault) {
        let idReads=0, individualReads=0; const calls=[];
        const query=()=>{
            individualReads++;
            if(count<=1000&&fault!=='filtered')fail('individual_read','small inbox must use bulk metadata');
            return Array.from({length:count},(_,i)=>({id:()=>i+1,messageId:()=>'m'+i,
                subject:()=>'s'+i,sender:()=>'a@example.com',dateReceived:()=>new Date('2026-10-16T00:00:00Z'),readStatus:()=>true}));
        };
        for(const name of ['id','messageId','subject','sender','dateReceived','readStatus','deletedStatus'])query[name]=()=>{
            calls.push(name);
            if(name==='id')idReads++;
            const n=fault==='short'&&name==='sender'?count-1:count;
            return Array.from({length:n},(_,i)=> name==='id'?i+1+(fault==='changed'&&idReads>1?1:0):
                name==='dateReceived'?new Date('2026-10-16T00:00:00Z'):name==='deletedStatus'?(fault==='deleted'&&i===0):name==='readStatus'?true:name==='sender'?'a@example.com':name+i);
        };
        query.whose=()=>query;const box={messages:query};
        const env={mail:{accounts:{whose:()=>()=>[{mailboxes:{whose:()=>()=>[box]}}]}}};
        return {env,calls,individualReads:()=>individualReads};
    }
    const searchArgs={account_id:'a',mailbox_path:['Inbox'],offset:500,limit:50};
    const bulk=searchFixture(550), found=executeMail({action:'mac_mail_search',args:searchArgs},bulk.env);
    assert(found.items.length===50&&found.items[0].id==='501'&&found.items[49].id==='550','bulk search preserves pagination and ids');
    assert(found.total_matches===550&&found.next_offset===null&&!found.truncated,'bulk search reports complete page');
    assert(bulk.individualReads()===0&&bulk.calls.length<=8,'550 messages use bounded property requests, never per-message reads');
    const snapshot=executeMail({action:'mac_mail_search',args:{...searchArgs,offset:0,limit:1000}},searchFixture(550).env);
    assert(snapshot.items.length===550&&snapshot.next_offset===null,'bounded inbox is read in one request');
    rejects(()=>executeMail({action:'mac_mail_search',args:searchArgs},searchFixture(550,'changed').env),'conflict');
    rejects(()=>executeMail({action:'mac_mail_search',args:searchArgs},searchFixture(550,'short').env),'invalid_response');
    const empty=executeMail({action:'mac_mail_search',args:{...searchArgs,offset:0}},searchFixture(0).env);
    assert(empty.items.length===0&&empty.total_matches===0&&empty.next_offset===null,'empty inbox remains an empty result');
    const large=searchFixture(1001), bounded=executeMail({action:'mac_mail_search',args:{...searchArgs,offset:0,limit:1000}},large.env);
    assert(bounded.items.length===50&&bounded.total_matches===1001&&large.individualReads()===1,
        'large mailboxes keep bounded page metadata instead of loading all columns');
    const deleted=executeMail({action:'mac_mail_search',args:{...searchArgs,offset:0,limit:1000}},searchFixture(550,'deleted').env);
    assert(deleted.items.length===549&&deleted.items[0].id==='2','bulk snapshot excludes deleted messages before applying pagination');
    const filtered=searchFixture(100,'filtered');
    const selected=executeMail({action:'mac_mail_search',args:{...searchArgs,offset:0,limit:1000,query:'s'}},filtered.env);
    assert(selected.items.length===50&&selected.next_offset===50&&filtered.calls.length===0,
        'filtered searches use the existing bounded path without re-evaluating predicates for every column');
    function fixture(accepted) {
        const draft = {draft_id:'draft-0001',sender:'a@example.com',subject:'subject',body:'已审核正文',to:['b@example.com'],cc:[],bcc:[],attachments:[],source:null};
        const owner={status:'draft',data:draft},state={'mail-draft-draft-0001.json':owner},events=[];
        const env={hash:x=>JSON.stringify(canonical(x)),read:n=>state[n],write:(n,v)=>{events.push(n);state[n]=v;},
            deliver:()=>{events.push('send');return accepted;}};
        return {env:env,state:state,events:events,draft:draft,args:{draft_id:'draft-0001',expected_revision:draftRecord(env,owner).revision,request_id:'request-0001'}};
    }
    const good = fixture(true), result = sendDraft(good.env,good.args);
    assert(result.status === 'handed_off' && result.delivered === false, 'handoff does not mean delivered');
    assert(good.events.indexOf('send') === 2, 'persist request and draft lock BEFORE send');
    assert(sendDraft(good.env,good.args).replayed === true && good.events.filter(x=>x==='send').length === 1, 'same request does not resend');
    rejects(()=>sendDraft(good.env,Object.assign({},good.args,{expected_revision:'changed'})),'request_conflict');
    rejects(()=>sendDraft(good.env,Object.assign({},good.args,{request_id:'request-0002'})),'outcome_unknown');
    const unknown = fixture(false);
    rejects(()=>sendDraft(unknown.env,unknown.args),'outcome_unknown');
    rejects(()=>sendDraft(unknown.env,unknown.args),'outcome_unknown');
    rejects(()=>sendDraft(unknown.env,Object.assign({},unknown.args,{request_id:'request-0002'})),'outcome_unknown');
    assert(unknown.events.filter(x=>x==='send').length === 1,'unknown outcome cannot resend with a new id');
    const stale = fixture(true); stale.draft.subject = 'changed';
    rejects(()=>sendDraft(stale.env,stale.args),'conflict'); assert(stale.events.length === 0,'stale draft never sends');
    const missing = fixture(true); delete missing.state['mail-draft-draft-0001.json'];
    rejects(()=>sendDraft(missing.env,missing.args),'not_managed');
    const disk = fixture(true); disk.env.write = ()=>fail('state_failed','disk full');
    rejects(()=>sendDraft(disk.env,disk.args),'state_failed'); assert(disk.events.length===0,'receipt failure never sends');
    function threadFixture() {
        const make = (id, mid, headers) => ({id:()=>id,messageId:()=>mid,allHeaders:()=>headers,
            dateReceived:()=>new Date('2026-10-16T00:00:00Z'),subject:()=>'same subject',sender:()=>'a@example.com',
            readStatus:()=>true,content:()=>'',toRecipients:()=>[],ccRecipients:()=>[],mailAttachments:()=>[]});
        const first=make(1,'a@x','Message-ID: <a@x>');
        const reply=make(2,'b@x','Message-ID: <b@x>\nIn-Reply-To: <a@x>');
        const last=make(3,'c@x','Message-ID: <c@x>\nIn-Reply-To: <b@x>');
        const unrelated=make(4,'d@x','Message-ID: <d@x>\nFrom: <a@x>');
        const boxes=[];
        [['Inbox',[first,reply,unrelated]],['Sent',[first,last]]].forEach(([name,items])=>{
            const messages=()=>fail('unfiltered_scan','thread must query matching headers instead of loading every message');
            messages.whose=predicate=>()=>items.filter(m=>{
                if(predicate.id!==undefined)return m.id()===predicate.id;
                return (predicate._or||[predicate]).some(p=>m.allHeaders().includes(p.allHeaders._contains));
            });
            boxes.push({name:()=>name,mailboxes:()=>[],messages:messages});
        });
        const mailboxes=()=>boxes;mailboxes.whose=p=>()=>boxes.filter(b=>b.name()===p.name);
        return {mail:{accounts:{whose:()=>()=>[{mailboxes:mailboxes}]}},hash:x=>JSON.stringify(x)};
    }
    const linked=executeMail({action:'mac_mail_thread',args:{account_id:'account',mailbox_path:['Inbox'],id:'1'}},threadFixture());
    assert(linked.items.length===3,'filtered thread query finds transitive replies and removes duplicates');
    assert(linked.complete_in_account===true && linked.body_read_required===true,'complete metadata still requires reading each body');
    const realNow=Date.now;let clockReads=0,limited;
    try {
        Date.now=()=>clockReads++===0?0:18000;
        limited=executeMail({action:'mac_mail_thread',args:{account_id:'account',mailbox_path:['Inbox'],id:'1'}},threadFixture());
    } finally { Date.now=realNow; }
    assert(limited.truncated===true && limited.degraded===true && limited.complete_in_account===false && limited.items.length===1 && limited.reason,
        'time limit preserves anchor and never claims a complete thread');
    function compositionFixture(corruptBody) {
        const values={},draft={};
        for(const key of ['sender','subject','content']) {
            const read=()=>values[key];
            if(key==='content')read.attachments={push:()=>fail('unexpected_attachment','no attachments in this fixture')};
            Object.defineProperty(draft,key,{get:()=>read,set:value=>{
                values[key]=key==='content' ? value.replace(/\u2028/g,'\n').replace(/^[ \t]+$/gm,'')+(corruptBody?'changed':'') : value;
            }});
        }
        for(const key of ['to','cc','bcc']) {
            const items=[],recipients=()=>items;recipients.push=item=>items.push(item);draft[key+'Recipients']=recipients;
        }
        const recipient=data=>({address:()=>data.address});
        return {mail:{OutgoingMessage:()=>draft,outgoingMessages:{push:()=>{}},ToRecipient:recipient,CcRecipient:recipient,BccRecipient:recipient,
            delete:()=>fail('unexpected_delete','new message has no recipients')}};
    }
    const composed={mode:'new',sender:'a@example.com',subject:'test',body:'第一段\n \n  保留缩进\n末段',to:['b@example.com'],cc:[],bcc:[],attachments:[]};
    const ready=prepareMessage(compositionFixture(false),composed);
    assert(ready.content()==='第一段\n\n  保留缩进\n末段','Mail blank-line cleanup preserves actual text and indentation');
    const separated=prepareMessage(compositionFixture(false),Object.assign({},composed,{body:'第一段\u2028第二段\n \n  保留缩进'}));
    assert(separated.content()==='第一段\n第二段\n\n  保留缩进','Mail Unicode line separator normalization preserves line breaks');
    rejects(()=>prepareMessage(compositionFixture(true),composed),'outcome_unknown');
    const outboxFixture=fixture(true);
    outboxFixture.env.files=()=>Object.keys(outboxFixture.state);
    const listRequest={action:'mac_mail_outbox',args:{query:'subject'}};
    const listed=executeMail(listRequest,outboxFixture.env);
    assert(listed.items.length===1&&listed.items[0].status==='draft'&&!listed.items[0].body,'outbox lists metadata without reading Mail or exposing whole bodies');
    const inspect={action:'mac_mail_outbox',args:{draft_id:'draft-0001'}};
    assert(executeMail(inspect,outboxFixture.env).body==='已审核正文','outbox can recover a draft across sessions');
    sendDraft(outboxFixture.env,outboxFixture.args);
    const handed=executeMail(inspect,outboxFixture.env);
    assert(handed.status==='handed_off'&&handed.delivery_confirmed===false&&!handed.revision,'handoff remains inspectable but never becomes a sendable draft or delivery proof');
    unknown.env.files=()=>Object.keys(unknown.state);
    assert(executeMail(inspect,unknown.env).status==='sending','unknown outcome stays locked and inspectable');
    assert(executeMail({action:'mac_mail_outbox',args:{query:'absent'}},outboxFixture.env).total_matches===0,'no matching draft is honest');
    rejects(()=>executeMail({action:'mac_mail_outbox',args:{draft_id:'../unsafe'}},outboxFixture.env),'bad_args');
    rejects(()=>executeMail({action:'mac_mail_outbox',args:{draft_id:'draft-0001',query:'subject'}},outboxFixture.env),'bad_args');
    const broken=fixture(true);broken.env.files=()=>Object.keys(broken.state);broken.state['mail-draft-draft-0001.json'].status='sent_maybe';
    rejects(()=>executeMail(listRequest,broken.env),'state_failed');
    const many=fixture(true);many.env.files=()=>Array.from({length:1001},(_,i)=>'mail-draft-fixture-'+i+'.json');
    rejects(()=>executeMail(listRequest,many.env),'too_large');
    const attachment=fixture(true),files={'/original.txt':'old','/replacement.txt':'new'},removed=[];
    attachment.env.uuid=()=> 'replacement-0001';attachment.env.filePath=n=>'/private/'+n;
    attachment.env.fileDigest=p=>{if(!(p in files))fail('bad_args','missing');return files[p];};
    attachment.env.copyFile=(a,b)=>{files[b]=files[a];};attachment.env.removeFile=p=>{removed.push(p);delete files[p];return true;};
    attachment.draft.attachments=[{path:'/original.txt',name:'original.txt',sha256:'old'}];
    const revision=draftRecord(attachment.env,attachment.state['mail-draft-draft-0001.json']).revision;
    const swap={action:'mac_mail_draft_update',args:{draft_id:'draft-0001',expected_revision:revision,attachments:['/replacement.txt']}};
    const replaced=executeMail(swap,attachment.env);
    assert(replaced.attachments.length===1&&replaced.attachments[0].sha256==='new'&&replaced.attachments[0].path!=='/replacement.txt','attachment replacement stores a verified private copy');
    assert(replaced.revision!==revision&&removed.includes('/original.txt')&&files['/replacement.txt']==='new','replacement invalidates old approval and preserves the source file');
    rejects(()=>executeMail(swap,attachment.env),'conflict');
    const oldState=JSON.stringify(attachment.state),oldPath=replaced.attachments[0].path;
    rejects(()=>executeMail({action:'mac_mail_draft_update',args:{draft_id:'draft-0001',expected_revision:replaced.revision,attachments:['/missing.txt']}},attachment.env),'bad_args');
    assert(JSON.stringify(attachment.state)===oldState&&files[oldPath]==='new','failed replacement preserves the existing draft and attachments');
    const cleared=executeMail({action:'mac_mail_draft_update',args:{draft_id:'draft-0001',expected_revision:replaced.revision,attachments:[]}},attachment.env);
    assert(cleared.attachments.length===0&&files['/replacement.txt']==='new','empty attachments explicitly removes copies, never original files');
    const pages=fixture(true);pages.env.files=()=>Object.keys(pages.state);
    pages.state['mail-draft-draft-0002.json']={status:'draft',data:{...pages.draft,draft_id:'draft-0002',body:'云'.repeat(8001)}};
    pages.state['mail-draft-discarded-0001.json']={status:'discarded',data:{draft_id:'discarded-0001'}};
    const firstPage=executeMail({action:'mac_mail_outbox',args:{limit:1}},pages.env);
    assert(firstPage.total_matches===2&&firstPage.next_offset===1&&firstPage.truncated,'outbox pagination explicitly reports omitted matches');
    const lastPage=executeMail({action:'mac_mail_outbox',args:{offset:1}},pages.env);
    assert(lastPage.items[0].draft_id==='draft-0002'&&lastPage.next_offset===null,'later page recovers the second exact draft');
    const tailPage=executeMail({action:'mac_mail_outbox',args:{draft_id:'draft-0002',offset:8000}},pages.env);
    assert(tailPage.body==='云'&&tailPage.next_offset===null,'outbox detail uses Unicode body pagination');
    assert(executeMail({action:'mac_mail_outbox',args:{status:'discarded'}},pages.env).total_matches===1,'discarded records require explicit selection');
    rejects(()=>executeMail({action:'mac_mail_outbox',args:{offset:100}},pages.env),'bad_args');
    pages.state['mail-draft-draft-0002.json'].data=null;
    rejects(()=>executeMail({action:'mac_mail_outbox',args:{query:'unrelated'}},pages.env),'state_failed');
    const diskSwap=fixture(true),diskFiles={'/new.txt':'new'};
    diskSwap.env.uuid=()=> 'disk-swap-0001';diskSwap.env.filePath=n=>'/private/'+n;
    diskSwap.env.fileDigest=p=>diskFiles[p];diskSwap.env.copyFile=(a,b)=>{diskFiles[b]=diskFiles[a];};
    diskSwap.env.removeFile=()=>fail('unexpected_delete','uncertain persisted draft must retain its attachments');
    diskSwap.env.write=(n,v)=>{diskSwap.state[n]=v;fail('state_failed','permissions failed after write');};
    rejects(()=>executeMail({action:'mac_mail_draft_update',args:{...diskSwap.args,attachments:['/new.txt']}},diskSwap.env),'outcome_unknown');
    const savedAfterError=diskSwap.state['mail-draft-draft-0001.json'];
    assert(diskFiles[savedAfterError.data.attachments[0].path]==='new','post-write failure preserves the new attachment referenced by the persisted draft');
    const legacy=fixture(true);legacy.env.files=()=>Object.keys(legacy.state);
    legacy.state['mail-draft-1.json']={pid:123,status:'draft'};
    const legacyList=executeMail(listRequest,legacy.env);
    assert(legacyList.items.length===1&&legacyList.degraded===true&&legacyList.unsupported_records.length===1&&legacyList.reason,
        'old process-bound draft markers are explicitly reported, never treated as sendable or silently discarded');
    return {passed:passed, real_email_sent:false};
}
JSON.stringify(selfTestMail());
