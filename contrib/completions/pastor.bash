_pastor() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="pastor"
                ;;
            pastor,agent)
                cmd="pastor__subcmd__agent"
                ;;
            pastor,bootstrap)
                cmd="pastor__subcmd__bootstrap"
                ;;
            pastor,bridge)
                cmd="pastor__subcmd__bridge"
                ;;
            pastor,bundle)
                cmd="pastor__subcmd__bundle"
                ;;
            pastor,completions)
                cmd="pastor__subcmd__completions"
                ;;
            pastor,config)
                cmd="pastor__subcmd__config"
                ;;
            pastor,connector)
                cmd="pastor__subcmd__connector"
                ;;
            pastor,events)
                cmd="pastor__subcmd__events"
                ;;
            pastor,fleet)
                cmd="pastor__subcmd__fleet"
                ;;
            pastor,flock)
                cmd="pastor__subcmd__flock"
                ;;
            pastor,head)
                cmd="pastor__subcmd__head"
                ;;
            pastor,help)
                cmd="pastor__subcmd__help"
                ;;
            pastor,inbox)
                cmd="pastor__subcmd__inbox"
                ;;
            pastor,job)
                cmd="pastor__subcmd__job"
                ;;
            pastor,join)
                cmd="pastor__subcmd__join"
                ;;
            pastor,leave)
                cmd="pastor__subcmd__leave"
                ;;
            pastor,limit)
                cmd="pastor__subcmd__limit"
                ;;
            pastor,login)
                cmd="pastor__subcmd__login"
                ;;
            pastor,machine)
                cmd="pastor__subcmd__machine"
                ;;
            pastor,orchestrator)
                cmd="pastor__subcmd__orchestrator"
                ;;
            pastor,profile)
                cmd="pastor__subcmd__profile"
                ;;
            pastor,queue)
                cmd="pastor__subcmd__queue"
                ;;
            pastor,serve)
                cmd="pastor__subcmd__serve"
                ;;
            pastor,setup)
                cmd="pastor__subcmd__setup"
                ;;
            pastor,status)
                cmd="pastor__subcmd__status"
                ;;
            pastor,task)
                cmd="pastor__subcmd__task"
                ;;
            pastor,tick)
                cmd="pastor__subcmd__tick"
                ;;
            pastor,trust)
                cmd="pastor__subcmd__trust"
                ;;
            pastor,watch)
                cmd="pastor__subcmd__watch"
                ;;
            pastor,workdir)
                cmd="pastor__subcmd__workdir"
                ;;
            pastor__subcmd__agent,help)
                cmd="pastor__subcmd__agent__subcmd__help"
                ;;
            pastor__subcmd__agent,signed-in)
                cmd="pastor__subcmd__agent__subcmd__signed__subcmd__in"
                ;;
            pastor__subcmd__agent__subcmd__help,help)
                cmd="pastor__subcmd__agent__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__agent__subcmd__help,signed-in)
                cmd="pastor__subcmd__agent__subcmd__help__subcmd__signed__subcmd__in"
                ;;
            pastor__subcmd__bundle,add)
                cmd="pastor__subcmd__bundle__subcmd__add"
                ;;
            pastor__subcmd__bundle,adopt)
                cmd="pastor__subcmd__bundle__subcmd__adopt"
                ;;
            pastor__subcmd__bundle,apply)
                cmd="pastor__subcmd__bundle__subcmd__apply"
                ;;
            pastor__subcmd__bundle,help)
                cmd="pastor__subcmd__bundle__subcmd__help"
                ;;
            pastor__subcmd__bundle,list)
                cmd="pastor__subcmd__bundle__subcmd__list"
                ;;
            pastor__subcmd__bundle,remove)
                cmd="pastor__subcmd__bundle__subcmd__remove"
                ;;
            pastor__subcmd__bundle,show)
                cmd="pastor__subcmd__bundle__subcmd__show"
                ;;
            pastor__subcmd__bundle__subcmd__help,add)
                cmd="pastor__subcmd__bundle__subcmd__help__subcmd__add"
                ;;
            pastor__subcmd__bundle__subcmd__help,adopt)
                cmd="pastor__subcmd__bundle__subcmd__help__subcmd__adopt"
                ;;
            pastor__subcmd__bundle__subcmd__help,apply)
                cmd="pastor__subcmd__bundle__subcmd__help__subcmd__apply"
                ;;
            pastor__subcmd__bundle__subcmd__help,help)
                cmd="pastor__subcmd__bundle__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__bundle__subcmd__help,list)
                cmd="pastor__subcmd__bundle__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__bundle__subcmd__help,remove)
                cmd="pastor__subcmd__bundle__subcmd__help__subcmd__remove"
                ;;
            pastor__subcmd__bundle__subcmd__help,show)
                cmd="pastor__subcmd__bundle__subcmd__help__subcmd__show"
                ;;
            pastor__subcmd__config,edit)
                cmd="pastor__subcmd__config__subcmd__edit"
                ;;
            pastor__subcmd__config,help)
                cmd="pastor__subcmd__config__subcmd__help"
                ;;
            pastor__subcmd__config__subcmd__help,edit)
                cmd="pastor__subcmd__config__subcmd__help__subcmd__edit"
                ;;
            pastor__subcmd__config__subcmd__help,help)
                cmd="pastor__subcmd__config__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__connector,describe)
                cmd="pastor__subcmd__connector__subcmd__describe"
                ;;
            pastor__subcmd__connector,help)
                cmd="pastor__subcmd__connector__subcmd__help"
                ;;
            pastor__subcmd__connector,install)
                cmd="pastor__subcmd__connector__subcmd__install"
                ;;
            pastor__subcmd__connector,link)
                cmd="pastor__subcmd__connector__subcmd__link"
                ;;
            pastor__subcmd__connector,list)
                cmd="pastor__subcmd__connector__subcmd__list"
                ;;
            pastor__subcmd__connector,try)
                cmd="pastor__subcmd__connector__subcmd__try"
                ;;
            pastor__subcmd__connector,uninstall)
                cmd="pastor__subcmd__connector__subcmd__uninstall"
                ;;
            pastor__subcmd__connector,unlink)
                cmd="pastor__subcmd__connector__subcmd__unlink"
                ;;
            pastor__subcmd__connector__subcmd__help,describe)
                cmd="pastor__subcmd__connector__subcmd__help__subcmd__describe"
                ;;
            pastor__subcmd__connector__subcmd__help,help)
                cmd="pastor__subcmd__connector__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__connector__subcmd__help,install)
                cmd="pastor__subcmd__connector__subcmd__help__subcmd__install"
                ;;
            pastor__subcmd__connector__subcmd__help,link)
                cmd="pastor__subcmd__connector__subcmd__help__subcmd__link"
                ;;
            pastor__subcmd__connector__subcmd__help,list)
                cmd="pastor__subcmd__connector__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__connector__subcmd__help,try)
                cmd="pastor__subcmd__connector__subcmd__help__subcmd__try"
                ;;
            pastor__subcmd__connector__subcmd__help,uninstall)
                cmd="pastor__subcmd__connector__subcmd__help__subcmd__uninstall"
                ;;
            pastor__subcmd__connector__subcmd__help,unlink)
                cmd="pastor__subcmd__connector__subcmd__help__subcmd__unlink"
                ;;
            pastor__subcmd__fleet,help)
                cmd="pastor__subcmd__fleet__subcmd__help"
                ;;
            pastor__subcmd__fleet,update)
                cmd="pastor__subcmd__fleet__subcmd__update"
                ;;
            pastor__subcmd__fleet__subcmd__help,help)
                cmd="pastor__subcmd__fleet__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__fleet__subcmd__help,update)
                cmd="pastor__subcmd__fleet__subcmd__help__subcmd__update"
                ;;
            pastor__subcmd__flock,add)
                cmd="pastor__subcmd__flock__subcmd__add"
                ;;
            pastor__subcmd__flock,default)
                cmd="pastor__subcmd__flock__subcmd__default"
                ;;
            pastor__subcmd__flock,describe)
                cmd="pastor__subcmd__flock__subcmd__describe"
                ;;
            pastor__subcmd__flock,edit)
                cmd="pastor__subcmd__flock__subcmd__edit"
                ;;
            pastor__subcmd__flock,help)
                cmd="pastor__subcmd__flock__subcmd__help"
                ;;
            pastor__subcmd__flock,join)
                cmd="pastor__subcmd__flock__subcmd__join"
                ;;
            pastor__subcmd__flock,leave)
                cmd="pastor__subcmd__flock__subcmd__leave"
                ;;
            pastor__subcmd__flock,list)
                cmd="pastor__subcmd__flock__subcmd__list"
                ;;
            pastor__subcmd__flock,remove)
                cmd="pastor__subcmd__flock__subcmd__remove"
                ;;
            pastor__subcmd__flock__subcmd__default,help)
                cmd="pastor__subcmd__flock__subcmd__default__subcmd__help"
                ;;
            pastor__subcmd__flock__subcmd__default,set)
                cmd="pastor__subcmd__flock__subcmd__default__subcmd__set"
                ;;
            pastor__subcmd__flock__subcmd__default,show)
                cmd="pastor__subcmd__flock__subcmd__default__subcmd__show"
                ;;
            pastor__subcmd__flock__subcmd__default__subcmd__help,help)
                cmd="pastor__subcmd__flock__subcmd__default__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__flock__subcmd__default__subcmd__help,set)
                cmd="pastor__subcmd__flock__subcmd__default__subcmd__help__subcmd__set"
                ;;
            pastor__subcmd__flock__subcmd__default__subcmd__help,show)
                cmd="pastor__subcmd__flock__subcmd__default__subcmd__help__subcmd__show"
                ;;
            pastor__subcmd__flock__subcmd__help,add)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__add"
                ;;
            pastor__subcmd__flock__subcmd__help,default)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__default"
                ;;
            pastor__subcmd__flock__subcmd__help,describe)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__describe"
                ;;
            pastor__subcmd__flock__subcmd__help,edit)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__edit"
                ;;
            pastor__subcmd__flock__subcmd__help,help)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__flock__subcmd__help,join)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__join"
                ;;
            pastor__subcmd__flock__subcmd__help,leave)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__leave"
                ;;
            pastor__subcmd__flock__subcmd__help,list)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__flock__subcmd__help,remove)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__remove"
                ;;
            pastor__subcmd__flock__subcmd__help__subcmd__default,set)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__default__subcmd__set"
                ;;
            pastor__subcmd__flock__subcmd__help__subcmd__default,show)
                cmd="pastor__subcmd__flock__subcmd__help__subcmd__default__subcmd__show"
                ;;
            pastor__subcmd__head,help)
                cmd="pastor__subcmd__head__subcmd__help"
                ;;
            pastor__subcmd__head,set)
                cmd="pastor__subcmd__head__subcmd__set"
                ;;
            pastor__subcmd__head,show)
                cmd="pastor__subcmd__head__subcmd__show"
                ;;
            pastor__subcmd__head,unset)
                cmd="pastor__subcmd__head__subcmd__unset"
                ;;
            pastor__subcmd__head__subcmd__help,help)
                cmd="pastor__subcmd__head__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__head__subcmd__help,set)
                cmd="pastor__subcmd__head__subcmd__help__subcmd__set"
                ;;
            pastor__subcmd__head__subcmd__help,show)
                cmd="pastor__subcmd__head__subcmd__help__subcmd__show"
                ;;
            pastor__subcmd__head__subcmd__help,unset)
                cmd="pastor__subcmd__head__subcmd__help__subcmd__unset"
                ;;
            pastor__subcmd__help,agent)
                cmd="pastor__subcmd__help__subcmd__agent"
                ;;
            pastor__subcmd__help,bootstrap)
                cmd="pastor__subcmd__help__subcmd__bootstrap"
                ;;
            pastor__subcmd__help,bridge)
                cmd="pastor__subcmd__help__subcmd__bridge"
                ;;
            pastor__subcmd__help,bundle)
                cmd="pastor__subcmd__help__subcmd__bundle"
                ;;
            pastor__subcmd__help,completions)
                cmd="pastor__subcmd__help__subcmd__completions"
                ;;
            pastor__subcmd__help,config)
                cmd="pastor__subcmd__help__subcmd__config"
                ;;
            pastor__subcmd__help,connector)
                cmd="pastor__subcmd__help__subcmd__connector"
                ;;
            pastor__subcmd__help,events)
                cmd="pastor__subcmd__help__subcmd__events"
                ;;
            pastor__subcmd__help,fleet)
                cmd="pastor__subcmd__help__subcmd__fleet"
                ;;
            pastor__subcmd__help,flock)
                cmd="pastor__subcmd__help__subcmd__flock"
                ;;
            pastor__subcmd__help,head)
                cmd="pastor__subcmd__help__subcmd__head"
                ;;
            pastor__subcmd__help,help)
                cmd="pastor__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__help,inbox)
                cmd="pastor__subcmd__help__subcmd__inbox"
                ;;
            pastor__subcmd__help,job)
                cmd="pastor__subcmd__help__subcmd__job"
                ;;
            pastor__subcmd__help,join)
                cmd="pastor__subcmd__help__subcmd__join"
                ;;
            pastor__subcmd__help,leave)
                cmd="pastor__subcmd__help__subcmd__leave"
                ;;
            pastor__subcmd__help,limit)
                cmd="pastor__subcmd__help__subcmd__limit"
                ;;
            pastor__subcmd__help,login)
                cmd="pastor__subcmd__help__subcmd__login"
                ;;
            pastor__subcmd__help,machine)
                cmd="pastor__subcmd__help__subcmd__machine"
                ;;
            pastor__subcmd__help,orchestrator)
                cmd="pastor__subcmd__help__subcmd__orchestrator"
                ;;
            pastor__subcmd__help,profile)
                cmd="pastor__subcmd__help__subcmd__profile"
                ;;
            pastor__subcmd__help,queue)
                cmd="pastor__subcmd__help__subcmd__queue"
                ;;
            pastor__subcmd__help,serve)
                cmd="pastor__subcmd__help__subcmd__serve"
                ;;
            pastor__subcmd__help,setup)
                cmd="pastor__subcmd__help__subcmd__setup"
                ;;
            pastor__subcmd__help,status)
                cmd="pastor__subcmd__help__subcmd__status"
                ;;
            pastor__subcmd__help,task)
                cmd="pastor__subcmd__help__subcmd__task"
                ;;
            pastor__subcmd__help,tick)
                cmd="pastor__subcmd__help__subcmd__tick"
                ;;
            pastor__subcmd__help,trust)
                cmd="pastor__subcmd__help__subcmd__trust"
                ;;
            pastor__subcmd__help,watch)
                cmd="pastor__subcmd__help__subcmd__watch"
                ;;
            pastor__subcmd__help,workdir)
                cmd="pastor__subcmd__help__subcmd__workdir"
                ;;
            pastor__subcmd__help__subcmd__agent,signed-in)
                cmd="pastor__subcmd__help__subcmd__agent__subcmd__signed__subcmd__in"
                ;;
            pastor__subcmd__help__subcmd__bundle,add)
                cmd="pastor__subcmd__help__subcmd__bundle__subcmd__add"
                ;;
            pastor__subcmd__help__subcmd__bundle,adopt)
                cmd="pastor__subcmd__help__subcmd__bundle__subcmd__adopt"
                ;;
            pastor__subcmd__help__subcmd__bundle,apply)
                cmd="pastor__subcmd__help__subcmd__bundle__subcmd__apply"
                ;;
            pastor__subcmd__help__subcmd__bundle,list)
                cmd="pastor__subcmd__help__subcmd__bundle__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__bundle,remove)
                cmd="pastor__subcmd__help__subcmd__bundle__subcmd__remove"
                ;;
            pastor__subcmd__help__subcmd__bundle,show)
                cmd="pastor__subcmd__help__subcmd__bundle__subcmd__show"
                ;;
            pastor__subcmd__help__subcmd__config,edit)
                cmd="pastor__subcmd__help__subcmd__config__subcmd__edit"
                ;;
            pastor__subcmd__help__subcmd__connector,describe)
                cmd="pastor__subcmd__help__subcmd__connector__subcmd__describe"
                ;;
            pastor__subcmd__help__subcmd__connector,install)
                cmd="pastor__subcmd__help__subcmd__connector__subcmd__install"
                ;;
            pastor__subcmd__help__subcmd__connector,link)
                cmd="pastor__subcmd__help__subcmd__connector__subcmd__link"
                ;;
            pastor__subcmd__help__subcmd__connector,list)
                cmd="pastor__subcmd__help__subcmd__connector__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__connector,try)
                cmd="pastor__subcmd__help__subcmd__connector__subcmd__try"
                ;;
            pastor__subcmd__help__subcmd__connector,uninstall)
                cmd="pastor__subcmd__help__subcmd__connector__subcmd__uninstall"
                ;;
            pastor__subcmd__help__subcmd__connector,unlink)
                cmd="pastor__subcmd__help__subcmd__connector__subcmd__unlink"
                ;;
            pastor__subcmd__help__subcmd__fleet,update)
                cmd="pastor__subcmd__help__subcmd__fleet__subcmd__update"
                ;;
            pastor__subcmd__help__subcmd__flock,add)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__add"
                ;;
            pastor__subcmd__help__subcmd__flock,default)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__default"
                ;;
            pastor__subcmd__help__subcmd__flock,describe)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__describe"
                ;;
            pastor__subcmd__help__subcmd__flock,edit)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__edit"
                ;;
            pastor__subcmd__help__subcmd__flock,join)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__join"
                ;;
            pastor__subcmd__help__subcmd__flock,leave)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__leave"
                ;;
            pastor__subcmd__help__subcmd__flock,list)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__flock,remove)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__remove"
                ;;
            pastor__subcmd__help__subcmd__flock__subcmd__default,set)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__default__subcmd__set"
                ;;
            pastor__subcmd__help__subcmd__flock__subcmd__default,show)
                cmd="pastor__subcmd__help__subcmd__flock__subcmd__default__subcmd__show"
                ;;
            pastor__subcmd__help__subcmd__head,set)
                cmd="pastor__subcmd__help__subcmd__head__subcmd__set"
                ;;
            pastor__subcmd__help__subcmd__head,show)
                cmd="pastor__subcmd__help__subcmd__head__subcmd__show"
                ;;
            pastor__subcmd__help__subcmd__head,unset)
                cmd="pastor__subcmd__help__subcmd__head__subcmd__unset"
                ;;
            pastor__subcmd__help__subcmd__inbox,send)
                cmd="pastor__subcmd__help__subcmd__inbox__subcmd__send"
                ;;
            pastor__subcmd__help__subcmd__job,describe)
                cmd="pastor__subcmd__help__subcmd__job__subcmd__describe"
                ;;
            pastor__subcmd__help__subcmd__job,disable)
                cmd="pastor__subcmd__help__subcmd__job__subcmd__disable"
                ;;
            pastor__subcmd__help__subcmd__job,edit)
                cmd="pastor__subcmd__help__subcmd__job__subcmd__edit"
                ;;
            pastor__subcmd__help__subcmd__job,enable)
                cmd="pastor__subcmd__help__subcmd__job__subcmd__enable"
                ;;
            pastor__subcmd__help__subcmd__job,list)
                cmd="pastor__subcmd__help__subcmd__job__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__job,reload)
                cmd="pastor__subcmd__help__subcmd__job__subcmd__reload"
                ;;
            pastor__subcmd__help__subcmd__job,run)
                cmd="pastor__subcmd__help__subcmd__job__subcmd__run"
                ;;
            pastor__subcmd__help__subcmd__join,create)
                cmd="pastor__subcmd__help__subcmd__join__subcmd__create"
                ;;
            pastor__subcmd__help__subcmd__join,list)
                cmd="pastor__subcmd__help__subcmd__join__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__join,revoke)
                cmd="pastor__subcmd__help__subcmd__join__subcmd__revoke"
                ;;
            pastor__subcmd__help__subcmd__limit,clear)
                cmd="pastor__subcmd__help__subcmd__limit__subcmd__clear"
                ;;
            pastor__subcmd__help__subcmd__limit,list)
                cmd="pastor__subcmd__help__subcmd__limit__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__login,cancel)
                cmd="pastor__subcmd__help__subcmd__login__subcmd__cancel"
                ;;
            pastor__subcmd__help__subcmd__login,code)
                cmd="pastor__subcmd__help__subcmd__login__subcmd__code"
                ;;
            pastor__subcmd__help__subcmd__login,list)
                cmd="pastor__subcmd__help__subcmd__login__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__machine,add)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__add"
                ;;
            pastor__subcmd__help__subcmd__machine,authorized-key)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__authorized__subcmd__key"
                ;;
            pastor__subcmd__help__subcmd__machine,bootstrap)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__bootstrap"
                ;;
            pastor__subcmd__help__subcmd__machine,check)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__check"
                ;;
            pastor__subcmd__help__subcmd__machine,cordon)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__cordon"
                ;;
            pastor__subcmd__help__subcmd__machine,describe)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__describe"
                ;;
            pastor__subcmd__help__subcmd__machine,install)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__install"
                ;;
            pastor__subcmd__help__subcmd__machine,list)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__machine,move)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__move"
                ;;
            pastor__subcmd__help__subcmd__machine,open)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__open"
                ;;
            pastor__subcmd__help__subcmd__machine,remove)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__remove"
                ;;
            pastor__subcmd__help__subcmd__machine,uncordon)
                cmd="pastor__subcmd__help__subcmd__machine__subcmd__uncordon"
                ;;
            pastor__subcmd__help__subcmd__orchestrator,describe)
                cmd="pastor__subcmd__help__subcmd__orchestrator__subcmd__describe"
                ;;
            pastor__subcmd__help__subcmd__orchestrator,disable)
                cmd="pastor__subcmd__help__subcmd__orchestrator__subcmd__disable"
                ;;
            pastor__subcmd__help__subcmd__orchestrator,enable)
                cmd="pastor__subcmd__help__subcmd__orchestrator__subcmd__enable"
                ;;
            pastor__subcmd__help__subcmd__orchestrator,list)
                cmd="pastor__subcmd__help__subcmd__orchestrator__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__orchestrator,note)
                cmd="pastor__subcmd__help__subcmd__orchestrator__subcmd__note"
                ;;
            pastor__subcmd__help__subcmd__orchestrator,run)
                cmd="pastor__subcmd__help__subcmd__orchestrator__subcmd__run"
                ;;
            pastor__subcmd__help__subcmd__orchestrator,start)
                cmd="pastor__subcmd__help__subcmd__orchestrator__subcmd__start"
                ;;
            pastor__subcmd__help__subcmd__orchestrator,stop)
                cmd="pastor__subcmd__help__subcmd__orchestrator__subcmd__stop"
                ;;
            pastor__subcmd__help__subcmd__profile,describe)
                cmd="pastor__subcmd__help__subcmd__profile__subcmd__describe"
                ;;
            pastor__subcmd__help__subcmd__profile,list)
                cmd="pastor__subcmd__help__subcmd__profile__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__profile,render)
                cmd="pastor__subcmd__help__subcmd__profile__subcmd__render"
                ;;
            pastor__subcmd__help__subcmd__queue,move)
                cmd="pastor__subcmd__help__subcmd__queue__subcmd__move"
                ;;
            pastor__subcmd__help__subcmd__serve,status)
                cmd="pastor__subcmd__help__subcmd__serve__subcmd__status"
                ;;
            pastor__subcmd__help__subcmd__serve,stop)
                cmd="pastor__subcmd__help__subcmd__serve__subcmd__stop"
                ;;
            pastor__subcmd__help__subcmd__setup,launchd)
                cmd="pastor__subcmd__help__subcmd__setup__subcmd__launchd"
                ;;
            pastor__subcmd__help__subcmd__setup,systemd)
                cmd="pastor__subcmd__help__subcmd__setup__subcmd__systemd"
                ;;
            pastor__subcmd__help__subcmd__task,attach)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__attach"
                ;;
            pastor__subcmd__help__subcmd__task,close)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__close"
                ;;
            pastor__subcmd__help__subcmd__task,describe)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__describe"
                ;;
            pastor__subcmd__help__subcmd__task,done)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__done"
                ;;
            pastor__subcmd__help__subcmd__task,list)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__task,priority)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__priority"
                ;;
            pastor__subcmd__help__subcmd__task,prune)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__prune"
                ;;
            pastor__subcmd__help__subcmd__task,read)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__read"
                ;;
            pastor__subcmd__help__subcmd__task,retry)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__retry"
                ;;
            pastor__subcmd__help__subcmd__task,run)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__run"
                ;;
            pastor__subcmd__help__subcmd__task,send)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__send"
                ;;
            pastor__subcmd__help__subcmd__task,sweep)
                cmd="pastor__subcmd__help__subcmd__task__subcmd__sweep"
                ;;
            pastor__subcmd__help__subcmd__trust,add)
                cmd="pastor__subcmd__help__subcmd__trust__subcmd__add"
                ;;
            pastor__subcmd__help__subcmd__trust,list)
                cmd="pastor__subcmd__help__subcmd__trust__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__trust,remove)
                cmd="pastor__subcmd__help__subcmd__trust__subcmd__remove"
                ;;
            pastor__subcmd__help__subcmd__workdir,apply)
                cmd="pastor__subcmd__help__subcmd__workdir__subcmd__apply"
                ;;
            pastor__subcmd__help__subcmd__workdir,check)
                cmd="pastor__subcmd__help__subcmd__workdir__subcmd__check"
                ;;
            pastor__subcmd__help__subcmd__workdir,list)
                cmd="pastor__subcmd__help__subcmd__workdir__subcmd__list"
                ;;
            pastor__subcmd__help__subcmd__workdir,place)
                cmd="pastor__subcmd__help__subcmd__workdir__subcmd__place"
                ;;
            pastor__subcmd__help__subcmd__workdir,remove)
                cmd="pastor__subcmd__help__subcmd__workdir__subcmd__remove"
                ;;
            pastor__subcmd__inbox,help)
                cmd="pastor__subcmd__inbox__subcmd__help"
                ;;
            pastor__subcmd__inbox,send)
                cmd="pastor__subcmd__inbox__subcmd__send"
                ;;
            pastor__subcmd__inbox__subcmd__help,help)
                cmd="pastor__subcmd__inbox__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__inbox__subcmd__help,send)
                cmd="pastor__subcmd__inbox__subcmd__help__subcmd__send"
                ;;
            pastor__subcmd__job,describe)
                cmd="pastor__subcmd__job__subcmd__describe"
                ;;
            pastor__subcmd__job,disable)
                cmd="pastor__subcmd__job__subcmd__disable"
                ;;
            pastor__subcmd__job,edit)
                cmd="pastor__subcmd__job__subcmd__edit"
                ;;
            pastor__subcmd__job,enable)
                cmd="pastor__subcmd__job__subcmd__enable"
                ;;
            pastor__subcmd__job,help)
                cmd="pastor__subcmd__job__subcmd__help"
                ;;
            pastor__subcmd__job,list)
                cmd="pastor__subcmd__job__subcmd__list"
                ;;
            pastor__subcmd__job,reload)
                cmd="pastor__subcmd__job__subcmd__reload"
                ;;
            pastor__subcmd__job,run)
                cmd="pastor__subcmd__job__subcmd__run"
                ;;
            pastor__subcmd__job__subcmd__help,describe)
                cmd="pastor__subcmd__job__subcmd__help__subcmd__describe"
                ;;
            pastor__subcmd__job__subcmd__help,disable)
                cmd="pastor__subcmd__job__subcmd__help__subcmd__disable"
                ;;
            pastor__subcmd__job__subcmd__help,edit)
                cmd="pastor__subcmd__job__subcmd__help__subcmd__edit"
                ;;
            pastor__subcmd__job__subcmd__help,enable)
                cmd="pastor__subcmd__job__subcmd__help__subcmd__enable"
                ;;
            pastor__subcmd__job__subcmd__help,help)
                cmd="pastor__subcmd__job__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__job__subcmd__help,list)
                cmd="pastor__subcmd__job__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__job__subcmd__help,reload)
                cmd="pastor__subcmd__job__subcmd__help__subcmd__reload"
                ;;
            pastor__subcmd__job__subcmd__help,run)
                cmd="pastor__subcmd__job__subcmd__help__subcmd__run"
                ;;
            pastor__subcmd__join,create)
                cmd="pastor__subcmd__join__subcmd__create"
                ;;
            pastor__subcmd__join,help)
                cmd="pastor__subcmd__join__subcmd__help"
                ;;
            pastor__subcmd__join,list)
                cmd="pastor__subcmd__join__subcmd__list"
                ;;
            pastor__subcmd__join,revoke)
                cmd="pastor__subcmd__join__subcmd__revoke"
                ;;
            pastor__subcmd__join__subcmd__help,create)
                cmd="pastor__subcmd__join__subcmd__help__subcmd__create"
                ;;
            pastor__subcmd__join__subcmd__help,help)
                cmd="pastor__subcmd__join__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__join__subcmd__help,list)
                cmd="pastor__subcmd__join__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__join__subcmd__help,revoke)
                cmd="pastor__subcmd__join__subcmd__help__subcmd__revoke"
                ;;
            pastor__subcmd__limit,clear)
                cmd="pastor__subcmd__limit__subcmd__clear"
                ;;
            pastor__subcmd__limit,help)
                cmd="pastor__subcmd__limit__subcmd__help"
                ;;
            pastor__subcmd__limit,list)
                cmd="pastor__subcmd__limit__subcmd__list"
                ;;
            pastor__subcmd__limit__subcmd__help,clear)
                cmd="pastor__subcmd__limit__subcmd__help__subcmd__clear"
                ;;
            pastor__subcmd__limit__subcmd__help,help)
                cmd="pastor__subcmd__limit__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__limit__subcmd__help,list)
                cmd="pastor__subcmd__limit__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__login,cancel)
                cmd="pastor__subcmd__login__subcmd__cancel"
                ;;
            pastor__subcmd__login,code)
                cmd="pastor__subcmd__login__subcmd__code"
                ;;
            pastor__subcmd__login,help)
                cmd="pastor__subcmd__login__subcmd__help"
                ;;
            pastor__subcmd__login,list)
                cmd="pastor__subcmd__login__subcmd__list"
                ;;
            pastor__subcmd__login__subcmd__help,cancel)
                cmd="pastor__subcmd__login__subcmd__help__subcmd__cancel"
                ;;
            pastor__subcmd__login__subcmd__help,code)
                cmd="pastor__subcmd__login__subcmd__help__subcmd__code"
                ;;
            pastor__subcmd__login__subcmd__help,help)
                cmd="pastor__subcmd__login__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__login__subcmd__help,list)
                cmd="pastor__subcmd__login__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__machine,add)
                cmd="pastor__subcmd__machine__subcmd__add"
                ;;
            pastor__subcmd__machine,authorized-key)
                cmd="pastor__subcmd__machine__subcmd__authorized__subcmd__key"
                ;;
            pastor__subcmd__machine,bootstrap)
                cmd="pastor__subcmd__machine__subcmd__bootstrap"
                ;;
            pastor__subcmd__machine,check)
                cmd="pastor__subcmd__machine__subcmd__check"
                ;;
            pastor__subcmd__machine,cordon)
                cmd="pastor__subcmd__machine__subcmd__cordon"
                ;;
            pastor__subcmd__machine,describe)
                cmd="pastor__subcmd__machine__subcmd__describe"
                ;;
            pastor__subcmd__machine,help)
                cmd="pastor__subcmd__machine__subcmd__help"
                ;;
            pastor__subcmd__machine,install)
                cmd="pastor__subcmd__machine__subcmd__install"
                ;;
            pastor__subcmd__machine,list)
                cmd="pastor__subcmd__machine__subcmd__list"
                ;;
            pastor__subcmd__machine,move)
                cmd="pastor__subcmd__machine__subcmd__move"
                ;;
            pastor__subcmd__machine,open)
                cmd="pastor__subcmd__machine__subcmd__open"
                ;;
            pastor__subcmd__machine,remove)
                cmd="pastor__subcmd__machine__subcmd__remove"
                ;;
            pastor__subcmd__machine,uncordon)
                cmd="pastor__subcmd__machine__subcmd__uncordon"
                ;;
            pastor__subcmd__machine__subcmd__help,add)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__add"
                ;;
            pastor__subcmd__machine__subcmd__help,authorized-key)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__authorized__subcmd__key"
                ;;
            pastor__subcmd__machine__subcmd__help,bootstrap)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__bootstrap"
                ;;
            pastor__subcmd__machine__subcmd__help,check)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__check"
                ;;
            pastor__subcmd__machine__subcmd__help,cordon)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__cordon"
                ;;
            pastor__subcmd__machine__subcmd__help,describe)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__describe"
                ;;
            pastor__subcmd__machine__subcmd__help,help)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__machine__subcmd__help,install)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__install"
                ;;
            pastor__subcmd__machine__subcmd__help,list)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__machine__subcmd__help,move)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__move"
                ;;
            pastor__subcmd__machine__subcmd__help,open)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__open"
                ;;
            pastor__subcmd__machine__subcmd__help,remove)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__remove"
                ;;
            pastor__subcmd__machine__subcmd__help,uncordon)
                cmd="pastor__subcmd__machine__subcmd__help__subcmd__uncordon"
                ;;
            pastor__subcmd__orchestrator,describe)
                cmd="pastor__subcmd__orchestrator__subcmd__describe"
                ;;
            pastor__subcmd__orchestrator,disable)
                cmd="pastor__subcmd__orchestrator__subcmd__disable"
                ;;
            pastor__subcmd__orchestrator,enable)
                cmd="pastor__subcmd__orchestrator__subcmd__enable"
                ;;
            pastor__subcmd__orchestrator,help)
                cmd="pastor__subcmd__orchestrator__subcmd__help"
                ;;
            pastor__subcmd__orchestrator,list)
                cmd="pastor__subcmd__orchestrator__subcmd__list"
                ;;
            pastor__subcmd__orchestrator,note)
                cmd="pastor__subcmd__orchestrator__subcmd__note"
                ;;
            pastor__subcmd__orchestrator,run)
                cmd="pastor__subcmd__orchestrator__subcmd__run"
                ;;
            pastor__subcmd__orchestrator,start)
                cmd="pastor__subcmd__orchestrator__subcmd__start"
                ;;
            pastor__subcmd__orchestrator,stop)
                cmd="pastor__subcmd__orchestrator__subcmd__stop"
                ;;
            pastor__subcmd__orchestrator__subcmd__help,describe)
                cmd="pastor__subcmd__orchestrator__subcmd__help__subcmd__describe"
                ;;
            pastor__subcmd__orchestrator__subcmd__help,disable)
                cmd="pastor__subcmd__orchestrator__subcmd__help__subcmd__disable"
                ;;
            pastor__subcmd__orchestrator__subcmd__help,enable)
                cmd="pastor__subcmd__orchestrator__subcmd__help__subcmd__enable"
                ;;
            pastor__subcmd__orchestrator__subcmd__help,help)
                cmd="pastor__subcmd__orchestrator__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__orchestrator__subcmd__help,list)
                cmd="pastor__subcmd__orchestrator__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__orchestrator__subcmd__help,note)
                cmd="pastor__subcmd__orchestrator__subcmd__help__subcmd__note"
                ;;
            pastor__subcmd__orchestrator__subcmd__help,run)
                cmd="pastor__subcmd__orchestrator__subcmd__help__subcmd__run"
                ;;
            pastor__subcmd__orchestrator__subcmd__help,start)
                cmd="pastor__subcmd__orchestrator__subcmd__help__subcmd__start"
                ;;
            pastor__subcmd__orchestrator__subcmd__help,stop)
                cmd="pastor__subcmd__orchestrator__subcmd__help__subcmd__stop"
                ;;
            pastor__subcmd__profile,describe)
                cmd="pastor__subcmd__profile__subcmd__describe"
                ;;
            pastor__subcmd__profile,help)
                cmd="pastor__subcmd__profile__subcmd__help"
                ;;
            pastor__subcmd__profile,list)
                cmd="pastor__subcmd__profile__subcmd__list"
                ;;
            pastor__subcmd__profile,render)
                cmd="pastor__subcmd__profile__subcmd__render"
                ;;
            pastor__subcmd__profile__subcmd__help,describe)
                cmd="pastor__subcmd__profile__subcmd__help__subcmd__describe"
                ;;
            pastor__subcmd__profile__subcmd__help,help)
                cmd="pastor__subcmd__profile__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__profile__subcmd__help,list)
                cmd="pastor__subcmd__profile__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__profile__subcmd__help,render)
                cmd="pastor__subcmd__profile__subcmd__help__subcmd__render"
                ;;
            pastor__subcmd__queue,help)
                cmd="pastor__subcmd__queue__subcmd__help"
                ;;
            pastor__subcmd__queue,move)
                cmd="pastor__subcmd__queue__subcmd__move"
                ;;
            pastor__subcmd__queue__subcmd__help,help)
                cmd="pastor__subcmd__queue__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__queue__subcmd__help,move)
                cmd="pastor__subcmd__queue__subcmd__help__subcmd__move"
                ;;
            pastor__subcmd__serve,help)
                cmd="pastor__subcmd__serve__subcmd__help"
                ;;
            pastor__subcmd__serve,status)
                cmd="pastor__subcmd__serve__subcmd__status"
                ;;
            pastor__subcmd__serve,stop)
                cmd="pastor__subcmd__serve__subcmd__stop"
                ;;
            pastor__subcmd__serve__subcmd__help,help)
                cmd="pastor__subcmd__serve__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__serve__subcmd__help,status)
                cmd="pastor__subcmd__serve__subcmd__help__subcmd__status"
                ;;
            pastor__subcmd__serve__subcmd__help,stop)
                cmd="pastor__subcmd__serve__subcmd__help__subcmd__stop"
                ;;
            pastor__subcmd__setup,help)
                cmd="pastor__subcmd__setup__subcmd__help"
                ;;
            pastor__subcmd__setup,launchd)
                cmd="pastor__subcmd__setup__subcmd__launchd"
                ;;
            pastor__subcmd__setup,systemd)
                cmd="pastor__subcmd__setup__subcmd__systemd"
                ;;
            pastor__subcmd__setup__subcmd__help,help)
                cmd="pastor__subcmd__setup__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__setup__subcmd__help,launchd)
                cmd="pastor__subcmd__setup__subcmd__help__subcmd__launchd"
                ;;
            pastor__subcmd__setup__subcmd__help,systemd)
                cmd="pastor__subcmd__setup__subcmd__help__subcmd__systemd"
                ;;
            pastor__subcmd__task,attach)
                cmd="pastor__subcmd__task__subcmd__attach"
                ;;
            pastor__subcmd__task,close)
                cmd="pastor__subcmd__task__subcmd__close"
                ;;
            pastor__subcmd__task,describe)
                cmd="pastor__subcmd__task__subcmd__describe"
                ;;
            pastor__subcmd__task,done)
                cmd="pastor__subcmd__task__subcmd__done"
                ;;
            pastor__subcmd__task,help)
                cmd="pastor__subcmd__task__subcmd__help"
                ;;
            pastor__subcmd__task,list)
                cmd="pastor__subcmd__task__subcmd__list"
                ;;
            pastor__subcmd__task,priority)
                cmd="pastor__subcmd__task__subcmd__priority"
                ;;
            pastor__subcmd__task,prune)
                cmd="pastor__subcmd__task__subcmd__prune"
                ;;
            pastor__subcmd__task,read)
                cmd="pastor__subcmd__task__subcmd__read"
                ;;
            pastor__subcmd__task,retry)
                cmd="pastor__subcmd__task__subcmd__retry"
                ;;
            pastor__subcmd__task,run)
                cmd="pastor__subcmd__task__subcmd__run"
                ;;
            pastor__subcmd__task,send)
                cmd="pastor__subcmd__task__subcmd__send"
                ;;
            pastor__subcmd__task,sweep)
                cmd="pastor__subcmd__task__subcmd__sweep"
                ;;
            pastor__subcmd__task__subcmd__help,attach)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__attach"
                ;;
            pastor__subcmd__task__subcmd__help,close)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__close"
                ;;
            pastor__subcmd__task__subcmd__help,describe)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__describe"
                ;;
            pastor__subcmd__task__subcmd__help,done)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__done"
                ;;
            pastor__subcmd__task__subcmd__help,help)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__task__subcmd__help,list)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__task__subcmd__help,priority)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__priority"
                ;;
            pastor__subcmd__task__subcmd__help,prune)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__prune"
                ;;
            pastor__subcmd__task__subcmd__help,read)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__read"
                ;;
            pastor__subcmd__task__subcmd__help,retry)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__retry"
                ;;
            pastor__subcmd__task__subcmd__help,run)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__run"
                ;;
            pastor__subcmd__task__subcmd__help,send)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__send"
                ;;
            pastor__subcmd__task__subcmd__help,sweep)
                cmd="pastor__subcmd__task__subcmd__help__subcmd__sweep"
                ;;
            pastor__subcmd__trust,add)
                cmd="pastor__subcmd__trust__subcmd__add"
                ;;
            pastor__subcmd__trust,help)
                cmd="pastor__subcmd__trust__subcmd__help"
                ;;
            pastor__subcmd__trust,list)
                cmd="pastor__subcmd__trust__subcmd__list"
                ;;
            pastor__subcmd__trust,remove)
                cmd="pastor__subcmd__trust__subcmd__remove"
                ;;
            pastor__subcmd__trust__subcmd__help,add)
                cmd="pastor__subcmd__trust__subcmd__help__subcmd__add"
                ;;
            pastor__subcmd__trust__subcmd__help,help)
                cmd="pastor__subcmd__trust__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__trust__subcmd__help,list)
                cmd="pastor__subcmd__trust__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__trust__subcmd__help,remove)
                cmd="pastor__subcmd__trust__subcmd__help__subcmd__remove"
                ;;
            pastor__subcmd__workdir,apply)
                cmd="pastor__subcmd__workdir__subcmd__apply"
                ;;
            pastor__subcmd__workdir,check)
                cmd="pastor__subcmd__workdir__subcmd__check"
                ;;
            pastor__subcmd__workdir,help)
                cmd="pastor__subcmd__workdir__subcmd__help"
                ;;
            pastor__subcmd__workdir,list)
                cmd="pastor__subcmd__workdir__subcmd__list"
                ;;
            pastor__subcmd__workdir,place)
                cmd="pastor__subcmd__workdir__subcmd__place"
                ;;
            pastor__subcmd__workdir,remove)
                cmd="pastor__subcmd__workdir__subcmd__remove"
                ;;
            pastor__subcmd__workdir__subcmd__help,apply)
                cmd="pastor__subcmd__workdir__subcmd__help__subcmd__apply"
                ;;
            pastor__subcmd__workdir__subcmd__help,check)
                cmd="pastor__subcmd__workdir__subcmd__help__subcmd__check"
                ;;
            pastor__subcmd__workdir__subcmd__help,help)
                cmd="pastor__subcmd__workdir__subcmd__help__subcmd__help"
                ;;
            pastor__subcmd__workdir__subcmd__help,list)
                cmd="pastor__subcmd__workdir__subcmd__help__subcmd__list"
                ;;
            pastor__subcmd__workdir__subcmd__help,place)
                cmd="pastor__subcmd__workdir__subcmd__help__subcmd__place"
                ;;
            pastor__subcmd__workdir__subcmd__help,remove)
                cmd="pastor__subcmd__workdir__subcmd__help__subcmd__remove"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        pastor)
            opts="-h -V --skill --head --help --version serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__agent)
            opts="-h --head --help signed-in help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__agent__subcmd__help)
            opts="signed-in help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__agent__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__agent__subcmd__help__subcmd__signed__subcmd__in)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__agent__subcmd__signed__subcmd__in)
            opts="-h --machine --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bootstrap)
            opts="-h --name --head-key --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head-key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bridge)
            opts="-h --agent --shepherd --machine --gen --join --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --gen)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --join)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle)
            opts="-h --head --help add list show remove apply adopt help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__add)
            opts="-h --git --ref --path --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --git)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --ref)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --path)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__adopt)
            opts="-h --agent --dry-run --json --local --home --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --agent)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --local)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --home)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__apply)
            opts="-h --dry-run --json --local --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --local)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__help)
            opts="add list show remove apply adopt help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__help__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__help__subcmd__adopt)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__help__subcmd__apply)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__help__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__help__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__list)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__remove)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__bundle__subcmd__show)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__completions)
            opts="-h --head --help bash elvish fish powershell zsh"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__config)
            opts="-h --head --help edit help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__config__subcmd__edit)
            opts="-h --local --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__config__subcmd__help)
            opts="edit help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__config__subcmd__help__subcmd__edit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__config__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector)
            opts="-h --head --help install link uninstall unlink list describe try help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__describe)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__help)
            opts="install link uninstall unlink list describe try help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__help__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__help__subcmd__install)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__help__subcmd__link)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__help__subcmd__try)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__help__subcmd__uninstall)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__help__subcmd__unlink)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__install)
            opts="-h --ref --yes --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --ref)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__link)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__list)
            opts="-w -h --wide --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__try)
            opts="-h --job --since --head --help watch"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --job)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --since)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__uninstall)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__connector__subcmd__unlink)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__events)
            opts="-h --follow --task --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --task)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__fleet)
            opts="-h --head --help update help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__fleet__subcmd__help)
            opts="update help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__fleet__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__fleet__subcmd__help__subcmd__update)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__fleet__subcmd__update)
            opts="-h --machine --dry-run --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock)
            opts="-h --head --help list add join leave remove default edit describe help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__add)
            opts="-h --default --description --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --description)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__default)
            opts="-h --head --help show set help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__default__subcmd__help)
            opts="show set help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__default__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__default__subcmd__help__subcmd__set)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__default__subcmd__help__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__default__subcmd__set)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__default__subcmd__show)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__describe)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__edit)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help)
            opts="list add join leave remove default edit describe help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__default)
            opts="show set"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__default__subcmd__set)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__default__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__edit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__join)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__leave)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__help__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__join)
            opts="-h --max --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --max)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__leave)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__list)
            opts="-w -h --wide --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__flock__subcmd__remove)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__head)
            opts="-h --head --help set show unset help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__head__subcmd__help)
            opts="set show unset help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__head__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__head__subcmd__help__subcmd__set)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__head__subcmd__help__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__head__subcmd__help__subcmd__unset)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__head__subcmd__set)
            opts="-h --pastor --force --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --pastor)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__head__subcmd__show)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__head__subcmd__unset)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help)
            opts="serve task queue status machine flock tick job orchestrator config completions events watch setup bootstrap fleet connector bundle workdir profile limit agent login trust join bridge inbox head leave help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__agent)
            opts="signed-in"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__agent__subcmd__signed__subcmd__in)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__bootstrap)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__bridge)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__bundle)
            opts="add list show remove apply adopt"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__bundle__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__bundle__subcmd__adopt)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__bundle__subcmd__apply)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__bundle__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__bundle__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__bundle__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__completions)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__config)
            opts="edit"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__config__subcmd__edit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__connector)
            opts="install link uninstall unlink list describe try"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__connector__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__connector__subcmd__install)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__connector__subcmd__link)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__connector__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__connector__subcmd__try)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__connector__subcmd__uninstall)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__connector__subcmd__unlink)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__events)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__fleet)
            opts="update"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__fleet__subcmd__update)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock)
            opts="list add join leave remove default edit describe"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__default)
            opts="show set"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__default__subcmd__set)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__default__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__edit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__join)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__leave)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__flock__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__head)
            opts="set show unset"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__head__subcmd__set)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__head__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__head__subcmd__unset)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__inbox)
            opts="send"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__inbox__subcmd__send)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__job)
            opts="list enable disable run reload edit describe"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__job__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__job__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__job__subcmd__edit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__job__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__job__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__job__subcmd__reload)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__job__subcmd__run)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__join)
            opts="create list revoke"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__join__subcmd__create)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__join__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__join__subcmd__revoke)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__leave)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__limit)
            opts="list clear"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__limit__subcmd__clear)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__limit__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__login)
            opts="list code cancel"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__login__subcmd__cancel)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__login__subcmd__code)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__login__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine)
            opts="add bootstrap remove move cordon uncordon list describe check install open authorized-key"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__authorized__subcmd__key)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__bootstrap)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__check)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__cordon)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__install)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__move)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__open)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__machine__subcmd__uncordon)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__orchestrator)
            opts="list describe run start stop enable disable note"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__orchestrator__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__orchestrator__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__orchestrator__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__orchestrator__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__orchestrator__subcmd__note)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__orchestrator__subcmd__run)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__orchestrator__subcmd__start)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__orchestrator__subcmd__stop)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__profile)
            opts="list describe render"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__profile__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__profile__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__profile__subcmd__render)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__queue)
            opts="move"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__queue__subcmd__move)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__serve)
            opts="stop status"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__serve__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__serve__subcmd__stop)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__setup)
            opts="systemd launchd"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__setup__subcmd__launchd)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__setup__subcmd__systemd)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task)
            opts="run list describe read attach retry priority close prune sweep send done"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__attach)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__close)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__done)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__priority)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__prune)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__read)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__retry)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__run)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__send)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__task__subcmd__sweep)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__tick)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__trust)
            opts="list add remove"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__trust__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__trust__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__trust__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__watch)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__workdir)
            opts="place remove check list apply"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__workdir__subcmd__apply)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__workdir__subcmd__check)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__workdir__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__workdir__subcmd__place)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__help__subcmd__workdir__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__inbox)
            opts="-h --head --help send help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__inbox__subcmd__help)
            opts="send help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__inbox__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__inbox__subcmd__help__subcmd__send)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__inbox__subcmd__send)
            opts="-h --config-dir --session --mode --wait --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config-dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mode)
                    COMPREPLY=($(compgen -W "prompting bypass" -- "${cur}"))
                    return 0
                    ;;
                --wait)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job)
            opts="-h --head --help list enable disable run reload edit describe help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__describe)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__disable)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__edit)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__enable)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__help)
            opts="list enable disable run reload edit describe help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__help__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__help__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__help__subcmd__edit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__help__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__help__subcmd__reload)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__help__subcmd__run)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__list)
            opts="-w -h --wide --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__reload)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__job__subcmd__run)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__join)
            opts="-h --name --token-stdin --skip-bootstrap --json --head --help create list revoke help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__join__subcmd__create)
            opts="-h --flock --tag --replace --ttl --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --flock)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tag)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --replace)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --ttl)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__join__subcmd__help)
            opts="create list revoke help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__join__subcmd__help__subcmd__create)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__join__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__join__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__join__subcmd__help__subcmd__revoke)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__join__subcmd__list)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__join__subcmd__revoke)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__leave)
            opts="-h --dry-run --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__limit)
            opts="-h --head --help list clear help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__limit__subcmd__clear)
            opts="-h --model --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --model)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__limit__subcmd__help)
            opts="list clear help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__limit__subcmd__help__subcmd__clear)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__limit__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__limit__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__limit__subcmd__list)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__login)
            opts="-h --json --head --help list code cancel help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__login__subcmd__cancel)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__login__subcmd__code)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__login__subcmd__help)
            opts="list code cancel help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__login__subcmd__help__subcmd__cancel)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__login__subcmd__help__subcmd__code)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__login__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__login__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__login__subcmd__list)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine)
            opts="-h --head --help add bootstrap remove move cordon uncordon list describe check install open authorized-key help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__add)
            opts="-h --local --command --session --max-agents --job-slots --burst --tag --flock --description --herdr --agent-key --dry-run --bootstrap --host-key --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --command)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --session)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --max-agents)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --job-slots)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --burst)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tag)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --flock)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --description)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --agent-key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --host-key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__authorized__subcmd__key)
            opts="-h --key --shepherd --write --dry-run --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__bootstrap)
            opts="-h --host-key --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --host-key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__check)
            opts="-h --provisioner --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --provisioner)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__cordon)
            opts="-h --reason --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --reason)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__describe)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help)
            opts="add bootstrap remove move cordon uncordon list describe check install open authorized-key help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__authorized__subcmd__key)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__bootstrap)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__check)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__cordon)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__install)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__move)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__open)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__help__subcmd__uncordon)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__install)
            opts="-h --agent --kind --dry-run --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --agent)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --kind)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__list)
            opts="-w -h --flock --wide --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --flock)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__move)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__open)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__remove)
            opts="-h --herdr --dry-run --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__machine__subcmd__uncordon)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator)
            opts="-h --head --help list describe run start stop enable disable note help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__describe)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__disable)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__enable)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help)
            opts="list describe run start stop enable disable note help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help__subcmd__note)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help__subcmd__run)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help__subcmd__start)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__help__subcmd__stop)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__list)
            opts="-w -h --wide --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__note)
            opts="-h --name --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__run)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__start)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__orchestrator__subcmd__stop)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__profile)
            opts="-h --head --help list describe render help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__profile__subcmd__describe)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__profile__subcmd__help)
            opts="list describe render help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__profile__subcmd__help__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__profile__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__profile__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__profile__subcmd__help__subcmd__render)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__profile__subcmd__list)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__profile__subcmd__render)
            opts="-h --kind --machine --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --kind)
                    COMPREPLY=($(compgen -W "claude agy opencode codex" -- "${cur}"))
                    return 0
                    ;;
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__queue)
            opts="-h --flock --machine --json --head --help move help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --flock)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__queue__subcmd__help)
            opts="move help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__queue__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__queue__subcmd__help__subcmd__move)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__queue__subcmd__move)
            opts="-h --top --before --after --to --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --before)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --after)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --to)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__serve)
            opts="-f -h --foreground --head --help stop status help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__serve__subcmd__help)
            opts="stop status help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__serve__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__serve__subcmd__help__subcmd__status)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__serve__subcmd__help__subcmd__stop)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__serve__subcmd__status)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__serve__subcmd__stop)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__setup)
            opts="-h --head --help systemd launchd help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__setup__subcmd__help)
            opts="systemd launchd help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__setup__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__setup__subcmd__help__subcmd__launchd)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__setup__subcmd__help__subcmd__systemd)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__setup__subcmd__launchd)
            opts="-y -h --herdr --enable --start --now --stop --yes --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__setup__subcmd__systemd)
            opts="-y -h --herdr --enable --start --now --stop --yes --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__status)
            opts="-h --probe --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task)
            opts="-h --head --help run list describe read attach retry priority close prune sweep send done help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__attach)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__close)
            opts="-h --remove-worktree --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__describe)
            opts="-h --all-summaries --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__done)
            opts="-h --summary --summary-file --current --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --summary)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --summary-file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help)
            opts="run list describe read attach retry priority close prune sweep send done help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__attach)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__close)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__describe)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__done)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__priority)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__prune)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__read)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__retry)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__run)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__send)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__help__subcmd__sweep)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__list)
            opts="-w -h --job --flock --machine --blocked --done --all --wide --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --job)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --flock)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__priority)
            opts="-h --preempt --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__prune)
            opts="-h --done --failed --closed --older-than --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --older-than)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__read)
            opts="-h --lines --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --lines)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__retry)
            opts="-h --place --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --place)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__run)
            opts="-h --prompt-file --repo --flock --machine --agent --agent-arg --model --fallback --no-fallback --priority --preempt --now --profile --worktree --branch --tag --load --timeout --label --keep-pane --remove-worktree --no-fetch --workdir-set --place --role --summary --description --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --prompt-file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --repo)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --flock)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --agent)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --agent-arg)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --model)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --fallback)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --priority)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --profile)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --branch)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --tag)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --load)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --timeout)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --label)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --workdir-set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --place)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --role)
                    COMPREPLY=($(compgen -W "agent orchestrator" -- "${cur}"))
                    return 0
                    ;;
                --summary)
                    COMPREPLY=($(compgen -W "ask require off" -- "${cur}"))
                    return 0
                    ;;
                --description)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__send)
            opts="-h --key --no-enter --trust --pick --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --pick)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__task__subcmd__sweep)
            opts="-h --machine --yes --dry-run --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__tick)
            opts="-h --dry-run --job --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --job)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__trust)
            opts="-h --head --help list add remove help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__trust__subcmd__add)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__trust__subcmd__help)
            opts="list add remove help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__trust__subcmd__help__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__trust__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__trust__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__trust__subcmd__help__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__trust__subcmd__list)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__trust__subcmd__remove)
            opts="-h --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__watch)
            opts="-h --now --name --reset --all --json --interval --connector --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --name)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --interval)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --connector)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir)
            opts="-h --head --help place remove check list apply help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__apply)
            opts="-h --local --dry-run --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__check)
            opts="-h --id --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__help)
            opts="place remove check list apply help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__help__subcmd__apply)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__help__subcmd__check)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__help__subcmd__place)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__help__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__list)
            opts="-h --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__place)
            opts="-h --local --set --bundle --kind --machine --dry-run --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --set)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --bundle)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --kind)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        pastor__subcmd__workdir__subcmd__remove)
            opts="-h --local --id --machine --dry-run --json --head --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --machine)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --head)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _pastor -o nosort -o bashdefault -o default pastor
else
    complete -F _pastor -o bashdefault -o default pastor
fi

# Names (jobs, flocks, machines, tasks, connectors) come from pastor itself.
_pastor_names() {
    local cur="${COMP_WORDS[COMP_CWORD]}" names
    if names=$(pastor __complete bash -- "${COMP_WORDS[@]:1:COMP_CWORD-1}" "${cur}" 2>/dev/null); then
        [[ ${cur} == "=" ]] && cur=""
        # One name per line, kept whole: a flock or machine name may hold a
        # space, which compgen -W would split into two words. After
        # `--fallback sonnet,` pastor already puts `sonnet,` before each name.
        COMPREPLY=()
        local name
        while IFS= read -r name; do
            [[ -n ${name} && ${name} == "${cur}"* ]] && COMPREPLY+=( "${name}" )
        done <<< "${names}"
        return 0
    fi
    _pastor "$@"
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _pastor_names -o nosort -o bashdefault -o default pastor
else
    complete -F _pastor_names -o bashdefault -o default pastor
fi
