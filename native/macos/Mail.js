/* Mail 的参数只作数据传入；不拼接 AppleScript、命令或谓词源码。 */
function fail(code, message) { const e = new Error(message); e.code = code; throw e; }
function canonical(value) {
    if (Array.isArray(value)) return value.map(canonical);
    if (value && typeof value === 'object') {
        const result = {}; Object.keys(value).sort().forEach(k => { result[k] = canonical(value[k]); }); return result;
    }
    return value;
}
function nativeEnvironment(request) {
    ObjC.import('Foundation');
    ObjC.bindFunction('CC_SHA256', ['void *', ['void *', 'unsigned int', 'void *']]);
    const fm = $.NSFileManager.defaultManager;
    return {
        mail: Application('com.apple.mail'),
        uuid: () => ObjC.unwrap($.NSUUID.UUID.UUIDString).toLowerCase(),
        filePath: function(name) {
            const path=request.state_path+'/'+name,parent=path.slice(0,path.lastIndexOf('/'));
            if(!fm.createDirectoryAtPathWithIntermediateDirectoriesAttributesError(parent,true,$({NSFilePosixPermissions:448}),null))fail('state_failed','附件目录无法创建');
            return path;
        },
        copyFile: function (source, target) {
            if (!fm.copyItemAtPathToPathError(source, target, null)) fail('attachment_failed', '无法保存草稿附件副本');
            if (!fm.setAttributesOfItemAtPathError($({NSFilePosixPermissions: 384}), target, null)) fail('state_failed', '附件权限设置失败');
        },
        removeFile: function(path) {
            const removed=fm.removeItemAtPathError(path,null);
            if(removed)fm.removeItemAtPathError(path.slice(0,path.lastIndexOf('/')),null);
            return removed;
        },
        fileDigest: function (path) {
            const attributes = fm.attributesOfItemAtPathError(path, null);
            if (!attributes || ObjC.unwrap(attributes.objectForKey($.NSFileType)) !== ObjC.unwrap($.NSFileTypeRegular)
                || Number(ObjC.unwrap(attributes.objectForKey($.NSFileSize))) > 20000000) fail('bad_args', '草稿附件不可读或超过20MB，不能核对发送内容');
            const bytes = $.NSData.dataWithContentsOfFile(path), digest = $.NSMutableData.dataWithLength(32);
            if (!bytes) fail('bad_args', '草稿附件无法读取');
            $.CC_SHA256(bytes.bytes, bytes.length, digest.mutableBytes);
            return ObjC.unwrap(digest.base64EncodedStringWithOptions(0));
        },
        hash: function (value) {
            const bytes = $(JSON.stringify(canonical(value))).dataUsingEncoding($.NSUTF8StringEncoding);
            const digest = $.NSMutableData.dataWithLength(32);
            $.CC_SHA256(bytes.bytes, bytes.length, digest.mutableBytes);
            return ObjC.unwrap(digest.base64EncodedStringWithOptions(0));
        },
        files: function () {
            if (!fm.fileExistsAtPath(request.state_path)) return [];
            const names = fm.contentsOfDirectoryAtPathError(request.state_path, null);
            if (!names) fail('state_failed', '无法读取云熙发件记录');
            return ObjC.deepUnwrap(names);
        },
        read: function (name) {
            const path = request.state_path + '/' + name;
            if (!fm.fileExistsAtPath(path)) return null;
            const data = $.NSString.stringWithContentsOfFileEncodingError(path, $.NSUTF8StringEncoding, null);
            if (!data) fail('state_failed', '邮件回执无法读取，停止操作');
            try { return JSON.parse(ObjC.unwrap(data)); } catch (_) { fail('state_failed', '邮件回执损坏，停止操作'); }
        },
        write: function (name, value) {
            const path = request.state_path + '/' + name;
            if (!$(JSON.stringify(value)).writeToFileAtomicallyEncodingError(path, true, $.NSUTF8StringEncoding, null)) fail('state_failed', '邮件回执未保存，停止操作');
            if (!fm.setAttributesOfItemAtPathError($({NSFilePosixPermissions: 384}), path, null)) fail('state_failed', '邮件回执权限设置失败');
        }
    };
}
function requestID(value) {
    if (typeof value !== 'string' || !/^[A-Za-z0-9_-]{8,100}$/.test(value)) fail('bad_args', 'request_id须为8至100位字母数字_-');
    return value;
}
function numericID(value) {
    if (typeof value !== 'string' || !/^[0-9]{1,16}$/.test(value) || !Number.isSafeInteger(Number(value))) fail('bad_args', '邮件id不是有效整数');
    return Number(value);
}
function email(value) {
    if (typeof value !== 'string' || value.length > 254 || !/^[A-Za-z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?\.[A-Za-z]{2,63}$/.test(value)) fail('bad_args', '请使用明确的单一邮箱地址，不能只给姓名或显示名');
    return value;
}
function addresses(args) {
    let total = 0; const seen = new Set();
    ['to', 'cc', 'bcc'].forEach(key => {
        (args[key] || []).forEach(address => {
            email(address); const normalized = address.toLowerCase();
            if (seen.has(normalized)) fail('bad_args', '收件地址重复，请先确认收件人');
            seen.add(normalized); total++;
        });
    });
    if (total === 0 || total > 30 || !(args.to || []).length) fail('bad_args', '需要至少一个明确的to地址，总收件人最多30个');
}
function one(items, detail) {
    if (items.length !== 1) fail(items.length ? 'ambiguous' : 'not_found', detail);
    return items[0];
}
function collection(value) {
    // Mail把空的富文本附件集合返回missing value(null)，不能当作接口故障。
    if (value === null) return [];
    if (!Array.isArray(value)) fail('invalid_response', 'Mail返回的集合类型无效');
    return value;
}
function account(env, id) {
    if (id === 'local') return {mailboxes: env.mail.mailboxes};
    return one(env.mail.accounts.whose({id: id})(), '账户不存在或不唯一，请重新查询');
}
function mailbox(env, args, field) {
    const parts = args[field || 'mailbox_path'];
    if (!Array.isArray(parts) || !parts.length || parts.some(p => !p || p === '.' || p === '..')) fail('bad_args', '邮箱路径必须是实际列出的非空名称数组');
    let box = account(env, args.account_id);
    parts.forEach(name => { box = one(box.mailboxes.whose({name: name})(), '邮箱路径不存在或不唯一，请重新查询'); });
    return box;
}
function located(env, args) { return one(mailbox(env, args).messages.whose({id: numericID(args.id)})(), '邮件已移动或不存在，请重新搜索'); }
function walkMailboxes(account, visit) {
    let count = 0;
    function walk(parent, path) {
        parent.mailboxes().forEach(box => {
            if (++count > 200 || path.length >= 12) fail('too_large', '邮箱树超过安全扫描范围，未读取完整账户');
            const current = path.concat([String(box.name())]); visit(box, current); walk(box, current);
        });
    }
    walk(account, []);
}
function metadata(message, args) {
    const date = message.dateReceived();
    return {account_id: args.account_id, mailbox_path: args.mailbox_path, id: String(message.id()), message_id: String(message.messageId()),
            subject: String(message.subject()), sender: String(message.sender()), received_at: date instanceof Date ? date.toISOString() : null,
            read: Boolean(message.readStatus())};
}
function searchMetadata(box, args) {
    const offset=args.offset||0;
    if(!args.query&&!args.unread_only) {
        // 无筛选器的属性批量读取，避免 Mail 对每个属性重新执行整箱谓词。
        const query=box.messages, ids=collection(query.id());
        if(ids.length<=1000) {
            if(ids.some(id=>!Number.isSafeInteger(id)||id<0)||new Set(ids).size!==ids.length)fail('invalid_response','邮件身份集合无效');
            const columns={};
            if(ids.length)for(const name of ['messageId','subject','sender','dateReceived','readStatus','deletedStatus']) {
                columns[name]=collection(query[name]());
                if(columns[name].length!==ids.length)fail('invalid_response','邮件属性数量不一致，未返回部分结果');
            }
            const active=[];
            for(let i=0;i<ids.length;i++) {
                if(typeof columns.deletedStatus[i]!=='boolean')fail('invalid_response','删除状态类型无效');
                if(!columns.deletedStatus[i])active.push(i);
            }
            if(offset>active.length)fail('bad_args','搜索分页超出范围；邮箱可能已变化，请重新查询');
            const end=Math.min(offset+(args.limit||20),active.length),items=[];
            for(const i of active.slice(offset,end)) {
                const date=columns.dateReceived[i];
                if(!(date instanceof Date)||!Number.isFinite(date.getTime())||typeof columns.readStatus[i]!=='boolean'
                    ||['messageId','subject','sender'].some(name=>typeof columns[name][i]!=='string'))fail('invalid_response','邮件属性类型无效');
                items.push({account_id:args.account_id,mailbox_path:args.mailbox_path,id:String(ids[i]),
                    message_id:columns.messageId[i],subject:columns.subject[i],sender:columns.sender[i],
                    received_at:date.toISOString(),read:columns.readStatus[i]});
            }
            if(JSON.stringify(collection(query.id()))!==JSON.stringify(ids))fail('conflict','查询期间邮件身份或顺序变化，请重试');
            return {items:items,next_offset:end<active.length?end:null,truncated:end<active.length,total_matches:active.length,
                scope:{account_id:args.account_id,mailbox_path:args.mailbox_path},note:'元数据批量快照，不读取正文或更改已读状态'};
        }
    }
    // 筛选搜索和大型邮箱保留小分页，避免读取整箱属性。
    const predicates=[{deletedStatus:false}];
    if(args.query)predicates.push({_or:[{subject:{_contains:args.query}},{sender:{_contains:args.query}}]});
    if(args.unread_only)predicates.push({readStatus:false});
    const matches=collection(box.messages.whose(predicates.length===1?predicates[0]:{_and:predicates})());
    if(offset>matches.length)fail('bad_args','搜索分页超出范围；邮箱可能已变化，请重新查询');
    const end=Math.min(offset+Math.min(args.limit||20,50),matches.length);
    return {items:matches.slice(offset,end).map(m=>metadata(m,args)),next_offset:end<matches.length?end:null,
        truncated:end<matches.length,total_matches:matches.length,scope:{account_id:args.account_id,mailbox_path:args.mailbox_path},
        note:'实时邮箱分页，翻页期间有新邮件或移动时应重新查询'};
}
function attachmentList(message) {
    const attachments = collection(message.mailAttachments());
    if (attachments.length > 100) fail('too_large', '附件超过100个，未读取完整附件清单');
    return attachments.map(a => ({id: String(a.id()), name: String(a.name()), size_bytes: Number(a.fileSize()), downloaded: Boolean(a.downloaded())}));
}
function readMessage(env, args) {
    const message = located(env, args), data = metadata(message, args);
    const body = String(message.content()), headers = String(message.allHeaders());
    if (body.length > 2000000 || headers.length > 100000) fail('too_large', '邮件正文或头部过大，请在Mail查看；本次不能声称完整读取');
    data.to = collection(message.toRecipients()).map(r => String(r.address())); data.cc = collection(message.ccRecipients()).map(r => String(r.address()));
    data.attachments = attachmentList(message); data.body = body; data.headers = headers;
    data.revision = env.hash(data);
    return {message: message, data: data};
}
function checkedMessage(env, args) {
    const value = readMessage(env, args);
    if (value.data.revision !== args.expected_revision) fail('conflict', '邮件已变化，请重新读取后再操作');
    return value;
}
function page(data, offset) {
    offset = offset || 0;
    const content = Array.from(data.body), end = Math.min(offset + 8000, content.length);
    if (offset > content.length) fail('bad_args', '正文分页超出范围');
    const output = Object.assign({}, data, {body: content.slice(offset, end).join(''), offset: offset,
        next_offset: end < content.length ? end : null, total_characters: content.length});
    delete output.headers;
    return output;
}
function referenceIDs(headers) {
    const ids = new Set();
    const unfolded = headers.replace(/\r?\n[ \t]+/g, ' ');
    unfolded.split(/\r?\n/).forEach(line => {
        if (/^(message-id|references|in-reply-to):/i.test(line)) {
            (line.match(/<[^<>\s]{1,500}>/g) || []).forEach(id => ids.add(id));
        }
    });
    return ids;
}
function threadMembers(records, anchor) {
    const wanted = new Set(referenceIDs(anchor.headers));
    if (!wanted.size) fail('unsupported', '邮件没有可用的线程标识，不能仅按主题猜测邮件串');
    const selected = new Set(); let changed = true;
    while (changed) {
        changed = false;
        records.forEach((record, index) => {
            if (selected.has(index)) return;
            const ids = referenceIDs(record.headers);
            if (Array.from(ids).some(id => wanted.has(id))) {
                selected.add(index); ids.forEach(id => wanted.add(id)); changed = true;
            }
        });
    }
    const seen = new Set();
    return Array.from(selected).map(i => records[i]).filter(record => {
        const key = record.message_id || record.id;
        if (seen.has(key)) return false; seen.add(key); return true;
    }).sort((a, b) => String(a.received_at).localeCompare(String(b.received_at)));
}
function draftID(id) {
    if (typeof id !== 'string' || !/^[a-z0-9-]{8,100}$/.test(id)) fail('bad_args','草稿id无效');
    return id;
}
function draftRecord(env, owner) {
    const data = owner.data;
    data.attachments.forEach(a => { if (env.fileDigest(a.path) !== a.sha256) fail('conflict','草稿附件已变化或不可读，停止编辑和发送'); });
    return Object.assign({}, data, {revision: env.hash(data), storage: 'yunxi_local_draft', sent: false,
        attachment_verification: data.attachments.length ? '本机附件副本经SHA256校验；Mail不支持可靠的附件内容回读，发送只确认系统接受' : 'no_attachments'});
}
function loadDraft(env, id) {
    draftID(id);
    const owner = env.read('mail-draft-' + id + '.json');
    if (!owner) fail('not_managed','未找到云熙本机草稿');
    if (!owner.data || owner.data.draft_id !== id || !['draft','sending','handed_off','discarded'].includes(owner.status)) fail('state_failed','云熙发件记录损坏，停止操作');
    if (owner.status !== 'discarded' && (['sender','subject','body'].some(k=>typeof owner.data[k]!=='string')
        || ['to','cc','bcc'].some(k=>!Array.isArray(owner.data[k])||owner.data[k].some(v=>typeof v!=='string'))
        || !Array.isArray(owner.data.attachments))) fail('state_failed','云熙发件内容损坏，停止操作');
    return owner;
}
function ownedDraft(env, id, revision) {
    const owner = loadDraft(env,id);
    if (owner.status !== 'draft') fail('outcome_unknown','该草稿已发送、正在发送或已丢弃，不能再次执行');
    const data = draftRecord(env,owner);
    if (revision !== undefined && data.revision !== revision) fail('conflict','草稿内容已改变，请重新读取并重新确认');
    return {data:data,owner:owner};
}
function outbox(env,args) {
    const note='仅查询云熙本机草稿和发送记录，不是Mail发件箱。handed_off只表示Mail接受；sending结果未知，禁止重发。实际发出须另查Mail已发送，不能据此证明收件人收到。';
    if (args.draft_id !== undefined) {
        if (args.query !== undefined || args.status !== undefined || args.limit !== undefined) fail('bad_args','按draft_id读取时不接受列表筛选参数');
        const owner=loadDraft(env,args.draft_id);
        const data=owner.status==='draft'?draftRecord(env,owner):owner.data;
        return Object.assign({},owner.status==='discarded'?data:page(data,args.offset),
            {status:owner.status,delivery_confirmed:false,read_only:true,note:note});
    }
    const names=env.files().filter(n=>n.startsWith('mail-draft-')&&!n.startsWith('mail-draft-request-')&&n.endsWith('.json')).sort();
    // shortcut: 本机草稿扫描最多1000条，达到上限时应增加显式归档，不能静默丢掉旧发送记录。
    if(names.length>1000)fail('too_large','云熙发件记录超过1000条，请按已知draft_id读取；未返回不完整列表');
    const query=(args.query||'').toLowerCase(),items=[],unsupported=[];
    for(const name of names) {
        const id=name.slice('mail-draft-'.length,-5);
        if (/^[0-9]+$/.test(id)) {
            const legacy=env.read(name);
            if(!legacy||!Number.isSafeInteger(legacy.pid)||legacy.pid<=0||legacy.status!=='draft'||Object.keys(legacy).sort().join(',')!=='pid,status')fail('state_failed','旧版草稿标记损坏，停止查询');
            unsupported.push({record:name,reason:'旧版Mail临时对象标记没有收件人或正文，无法核实；未修改或发送'});continue;
        }
        const owner=loadDraft(env,id),d=owner.data;
        if(args.status?args.status!==owner.status:owner.status==='discarded')continue;
        if(query&&![d.subject||'',d.sender||'',...(d.to||[]),...(d.cc||[]),...(d.bcc||[])].some(s=>s.toLowerCase().includes(query)))continue;
        items.push({draft_id:id,status:owner.status,subject:d.subject||'',sender:d.sender||'',to:d.to||[],cc:d.cc||[],bcc:d.bcc||[],
            attachment_names:(d.attachments||[]).map(a=>a.name),source:d.source||null});
    }
    const offset=args.offset||0,end=Math.min(offset+(args.limit||20),items.length);
    if(offset>items.length)fail('bad_args','发件记录分页超出范围，请重新查询');
    return {items:items.slice(offset,end),total_matches:items.length,next_offset:end<items.length?end:null,truncated:end<items.length,
        read_only:true,degraded:unsupported.length>0,unsupported_records:unsupported,reason:unsupported.length?'存在无法核实的旧版临时草稿标记，列表只包含当前格式记录':'',
        note:note+' 列表按草稿id排列；编辑或发送前必须重新读取当前草稿。'};
}
function copyAttachments(env,paths,prefix) {
    const attachments=[],created=[];
    try {
        for(const path of paths) {
            const original=env.fileDigest(path),target=env.filePath(prefix+'-'+attachments.length+'/'+path.split('/').pop());
            env.copyFile(path,target);created.push(target);const hash=env.fileDigest(target);
            if(hash!==original)fail('conflict','复制时附件发生变化');
            attachments.push({path:target,name:path.split('/').pop(),sha256:hash});
        }
        return attachments;
    } catch(error) {
        const cleaned=created.map(p=>env.removeFile(p)).every(Boolean);
        if(!cleaned)fail('state_failed','附件准备失败且部分临时副本未清理；原草稿未替换');
        throw error;
    }
}
function changeRecipients(env, draft, args) {
    ['to','cc','bcc'].forEach(key=>{
        const recipients = draft[key+'Recipients'];
        collection(recipients()).forEach(r=>env.mail.delete(r));
        const type=key==='to'?'ToRecipient':key==='cc'?'CcRecipient':'BccRecipient';
        (args[key]||[]).forEach(address=>recipients.push(env.mail[type]({address:address})));
    });
}
function mailBody(value) {
    // Mail将Unicode行分隔符转为换行并清除空白行空格；保留正文字符、行数和文字行缩进。
    return value.replace(/\u2028/g, '\n').replace(/^[ \t]+$/gm, '').trim();
}
function prepareMessage(env, data) {
    const mail=env.mail; let draft;
    if (data.source && data.mode === 'reply') {
        const original=checkedMessage(env,data.source).message;
        draft=mail.reply(original,{openingWindow:false,replyToAll:false});
    } else {draft=mail.OutgoingMessage({visible:false});mail.outgoingMessages.push(draft);}
    draft.sender=data.sender;draft.subject=data.subject;draft.content=mailBody(data.body);
    changeRecipients(env,draft,data);
    ['to','cc','bcc'].forEach(key=>{
        const actual=collection(draft[key+'Recipients']()).map(r=>String(r.address()).toLowerCase()).sort();
        if(JSON.stringify(actual)!==JSON.stringify((data[key]||[]).map(s=>s.toLowerCase()).sort()))fail('outcome_unknown','Mail收件人读回不符，未调用发送');
    });
    if(String(draft.sender())!==data.sender||String(draft.subject())!==data.subject||mailBody(String(draft.content()))!==mailBody(data.body))fail('outcome_unknown','Mail正文或发件人读回不符，未调用发送');
    // shortcut: Mail读取富文本附件会报-10000；核对本机副本并检查插入回执，待系统提供可靠附件读回API后补强。
    data.attachments.forEach(a=>draft.content.attachments.push(mail.Attachment({fileName:Path(a.path)})));
    return draft;
}
function deliver(env, data) {
    // 不用Mail的不可核实附件回读充当内容证明；本机副本已在发送前校验。
    return env.mail.send(prepareMessage(env,data));
}
function sendDraft(env,args) {
    const request=requestID(args.request_id),name='mail-send-request-'+request+'.json',fingerprint=env.hash(args),receipt=env.read(name);
    if(receipt){
        if(receipt.fingerprint!==fingerprint)fail('request_conflict','request_id已用于不同发送参数');
        if(receipt.result)return Object.assign({},receipt.result,{replayed:true});
        fail('outcome_unknown','上次发送结果未知，请核对发件箱/已发送，禁止换编号重发');
    }
    const checked=ownedDraft(env,args.draft_id,args.expected_revision);
    addresses(checked.data);email(checked.data.sender);
    if(checked.data.source)checkedMessage(env,checked.data.source);
    env.write(name,{fingerprint:fingerprint});
    env.write('mail-draft-'+args.draft_id+'.json',Object.assign({},checked.owner,{status:'sending'}));
    if((env.deliver||deliver)(env,checked.data)!==true)fail('outcome_unknown','Mail未确认接受发送，请核对，不能自动重发');
    const limited=checked.data.attachments.length>0;
    const result={draft_id:args.draft_id,status:'handed_off',delivered:false,replayed:false,degraded:limited,
        reason:limited?'Mail不支持附件内容回读；已核对本机SHA256副本并提交附件插入，但回执不证明送达':''};
    env.write('mail-draft-'+args.draft_id+'.json',Object.assign({},checked.owner,{status:'handed_off'}));
    env.write(name,{fingerprint:fingerprint,result:result});return result;
}
function executeMail(request, env) {
    env.stage = request.action;
    const action = request.action, args = request.args, mail = env.mail;
    if (action === 'mac_mail_outbox') return outbox(env,args);
    if (action === 'mac_mail_accounts') {
        const items = mail.accounts().filter(a => Boolean(a.enabled())).map(a => {
            const boxes = []; walkMailboxes(a, (box, path) => { boxes.push({path: path, unread_count: Number(box.unreadCount())}); });
            return {id: String(a.id()), name: String(a.name()), addresses: a.emailAddresses().map(String), mailboxes: boxes};
        });
        const local = []; walkMailboxes(account(env,'local'), (box,path)=>local.push({path:path,unread_count:Number(box.unreadCount())}));
        const configured = items.length > 0;
        if (local.length) items.push({id:'local',name:'在我的Mac上',addresses:[],mailboxes:local});
        return {accounts: items, configured: configured};
    }
    if (action === 'mac_mail_search') return searchMetadata(mailbox(env,args),args);
    if (action === 'mac_mail_read') return page(readMessage(env, args).data, args.offset);
    if (action === 'mac_mail_thread') {
        const source = readMessage(env, args), anchor = source.data, wanted = referenceIDs(anchor.headers);
        if (!wanted.size) fail('unsupported', '邮件没有可用的线程标识，不能仅按主题猜测邮件串');
        if (wanted.size > 100) fail('too_large', '线程引用超过100个，未读取完整邮件串');
        const records = [Object.assign(metadata(source.message, args), {headers: anchor.headers})], boxes = [];
        const seen = new Set([JSON.stringify([args.mailbox_path, args.id])]);
        walkMailboxes(account(env, args.account_id), (box, path) => boxes.push({box: box, path: path}));
        let pending = Array.from(wanted), scanned = 0, truncated = false;
        const started = Date.now();
        while (pending.length && !truncated) {
            const predicates = pending.map(id => ({allHeaders: {_contains: id}})); pending = [];
            for (const entry of boxes) {
                if (Date.now() - started >= 18000) { truncated = true; break; }
                // 让Mail按头部筛候选；逐封读取整个账户会在真实邮箱中超过原生调用时限。
                const matches = entry.box.messages.whose(predicates.length === 1 ? predicates[0] : {_or: predicates})();
                for (const message of matches) {
                    if (++scanned > 2000 || Date.now() - started >= 18000) { truncated = true; break; }
                    const key = JSON.stringify([entry.path, String(message.id())]);
                    if (seen.has(key)) continue; seen.add(key);
                    const headers = String(message.allHeaders());
                    if (headers.length > 100000) fail('too_large', '邮件头部过大，未读取完整邮件串');
                    const ids = referenceIDs(headers);
                    if (!Array.from(ids).some(id => wanted.has(id))) continue;
                    records.push(Object.assign(metadata(message, {account_id: args.account_id, mailbox_path: entry.path}), {headers: headers}));
                    for (const id of ids) {
                        if (wanted.has(id)) continue;
                        if (wanted.size === 100) { truncated = true; break; }
                        wanted.add(id); pending.push(id);
                    }
                    if (truncated) break;
                }
                if (truncated) break;
            }
        }
        const items = threadMembers(records, anchor);
        if (items.length > 100) fail('too_large', '关联邮件超过100封，请缩小范围；未读完整邮件串');
        items.forEach(item => { delete item.headers; });
        return {items: items, truncated: truncated, complete_in_account: !truncated, degraded: truncated,
            reason: truncated ? '线程查询达到时间、候选数量或引用数量上限，结果不完整' : '',
            scope: '此Mac Mail已同步的单一账户，不包含其他账户或尚未同步的远端邮件',
            body_read_required: true, note: '逐封mac_mail_read并翻完正文分页后才能声称已阅读完整内容'};
    }
    if (action === 'mac_mail_mark_read') {
        const checked = checkedMessage(env, args); checked.message.readStatus = args.read;
        const after = readMessage(env, args).data;
        if (after.read !== args.read) fail('outcome_unknown', '标记后未读回预期状态，请先核对');
        return page(after, 0);
    }
    if (action === 'mac_mail_archive') {
        const forbidden = /^(trash|deleted messages|deleted items|junk|spam|废纸篓|垃圾箱|垃圾邮件)$/i;
        if (!args.archive_path.length || args.archive_path.some(p => forbidden.test(p))) fail('bad_args', '归档不能指向垃圾箱或废纸篓');
        const checked = checkedMessage(env, args), target = mailbox(env, args, 'archive_path');
        const originalID = checked.data.message_id;
        if (!originalID) fail('unsupported', '此邮件没有Message-ID，无法核对移动结果');
        if (JSON.stringify(args.mailbox_path) === JSON.stringify(args.archive_path)) return {archived: true, already_in_target: true, item: page(checked.data, 0)};
        const prior = target.messages.whose({messageId: originalID})();
        if (prior.length) fail('conflict', '目标邮箱已有相同Message-ID，先核对重复邮件，未移动');
        mail.move(checked.message, {to: target});
        const moved = one(target.messages.whose({messageId: originalID})(), '移动结果未知，请核对原邮箱和目标邮箱');
        const old = mailbox(env, args).messages.whose({id: numericID(args.id)})();
        if (old.length) fail('outcome_unknown', '原邮箱仍有邮件，不能确认归档完成');
        return {archived: true, item: metadata(moved, {account_id: args.account_id, mailbox_path: args.archive_path})};
    }
    if (action === 'mac_mail_attachment_save') {
        const checked = checkedMessage(env, args);
        const attachment = one(checked.message.mailAttachments.whose({id: args.attachment_id})(), '附件不存在或不唯一');
        if (Number(attachment.fileSize()) > 20000000) fail('too_large', '附件超过20MB，请在Mail处理');
        if (!request.attachment_staging) fail('installation_error', '缺少私有附件暂存路径');
        mail.save(attachment, {in: Path(request.attachment_staging)});
        return {staged: true, attachment_id: args.attachment_id, name: String(attachment.name())};
    }
    if (action === 'mac_mail_draft') {
        addresses(args); email(args.sender);
        const senders=mail.accounts().filter(a=>Boolean(a.enabled())).reduce((all,a)=>all.concat(a.emailAddresses().map(s=>String(s).toLowerCase())),[]);
        if(senders.indexOf(args.sender.toLowerCase())<0)fail('bad_args','发件地址不属于Mail已启用账户');
        const key='mail-draft-request-'+requestID(args.request_id)+'.json',fingerprint=env.hash(args),prior=env.read(key);
        if(prior){
            if(prior.fingerprint!==fingerprint)fail('request_conflict','request_id已用于不同起草内容');
            if(!prior.draft_id)fail('outcome_unknown','上次起草结果未知，请先核对');
            return Object.assign({},ownedDraft(env,prior.draft_id).data,{replayed:true});
        }
        let source=null;
        if(args.mode!=='new'){
            if(!args.source)fail('bad_args','回复或转发需要原邮件定位及revision');
            source=checkedMessage(env,args.source);
        }else if(args.source)fail('bad_args','新邮件不接受source');
        const id=draftID(env.uuid()),attachments=[];
        const data={draft_id:id,mode:args.mode,sender:args.sender,to:args.to,cc:args.cc||[],bcc:args.bcc||[],subject:args.subject,
            body:args.body+(source?'\n\n----- 原邮件 -----\nFrom: '+source.data.sender+'\nSubject: '+source.data.subject+'\nDate: '+source.data.received_at+'\n\n'+source.data.body:''),attachments:attachments,source:args.source||null};
        if(data.body.length>100000)fail('too_large','引用后的正文超过100000字符，请缩短后起草');
        env.write(key,{fingerprint:fingerprint});
        try {
            attachments.push(...copyAttachments(env,args.attachments||[],'mail-'+id));
            if(args.mode==='forward')collection(source.message.mailAttachments()).forEach(a=>{
                const name=String(a.name());
                if(!name||/[\/\\\x00]/.test(name)||Number(a.fileSize())>20000000)fail('unsupported','原邮件附件无法安全转存');
                const target=env.filePath('mail-'+id+'-'+attachments.length+'/'+name);
                mail.save(a,{in:Path(target)});
                attachments.push({path:target,name:name,sha256:env.fileDigest(target)});
            });
            const owner={status:'draft',data:data}; env.write('mail-draft-'+id+'.json',owner);
            env.write(key,{fingerprint:fingerprint,draft_id:id});
            return Object.assign({},draftRecord(env,owner),{replayed:false});
        }catch(e){attachments.forEach(a=>env.removeFile(a.path));throw e;}
    }
    if(action==='mac_mail_draft_get')return ownedDraft(env,args.draft_id).data;
    if(action==='mac_mail_draft_update'){
        if(Object.keys(args).length<=2)fail('bad_args','没有要修改的内容');
        const checked=ownedDraft(env,args.draft_id,args.expected_revision),data=Object.assign({},checked.owner.data);
        ['to','cc','bcc','subject','body'].forEach(k=>{if(args[k]!==undefined)data[k]=args[k];});
        addresses(data);
        const replaced=args.attachments!==undefined;
        if(replaced)data.attachments=copyAttachments(env,args.attachments,'mail-'+args.draft_id+'-'+draftID(env.uuid()));
        const owner={status:'draft',data:data};
        try { env.write('mail-draft-'+args.draft_id+'.json',owner); }
        catch(error) {
            // 原子写入之后也可能因权限失败报错；保留新旧副本，不能删掉可能已被保存的附件。
            fail('outcome_unknown','草稿保存结果未确认，附件副本已保留；请重新读取，不能直接发送');
        }
        const cleanup=!replaced||checked.owner.data.attachments.map(a=>env.removeFile(a.path)).every(Boolean);
        return Object.assign({},draftRecord(env,owner),{degraded:!cleanup,reason:cleanup?'':'草稿已更新，但部分旧附件副本清理失败'});
    }
    if(action==='mac_mail_draft_discard'){
        const checked=ownedDraft(env,args.draft_id,args.expected_revision);
        env.write('mail-draft-'+args.draft_id+'.json',{status:'discarded',data:{draft_id:args.draft_id}});
        const cleanup=checked.data.attachments.every(a=>env.removeFile(a.path));
        return {discarded:true,draft_id:args.draft_id,degraded:!cleanup,reason:cleanup?'':'草稿已丢弃，但部分私有附件副本清理失败'};
    }
    if (action === 'mac_mail_send') return sendDraft(env, args);
    fail('bad_args', '未知邮件动作');
}
function runRequest(request, environment) {
    let env;
    try { env = environment || nativeEnvironment(request); return {ok: true, result: executeMail(request, env)}; }
    catch (error) {
        return {ok: false, code: error.code || 'mail_error', message: error.code ? error.message :
            'Mail操作失败（阶段=' + (env ? env.stage : 'initialization') + '，系统代码=' + (Number(error.errorNumber || error.number) || 'unknown') + '）；可能部分完成，请先核对，不能盲目重试发送。'};
    }
}
