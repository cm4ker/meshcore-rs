package ru.ommesh.otg;

import android.animation.Animator;
import android.animation.AnimatorListenerAdapter;
import android.animation.ValueAnimator;
import android.content.Context;
import android.util.AttributeSet;
import android.view.MotionEvent;
import android.view.View;
import android.view.ViewConfiguration;
import android.view.ViewTreeObserver;
import android.view.animation.DecelerateInterpolator;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;

public final class JournalPanel extends LinearLayout {
    private final int touchSlop;
    private final int swipeDistance;
    private View header;
    private View body;
    private View preview;
    private ScrollView log;
    private Button toggle;
    private ValueAnimator animation;
    private boolean expanded;
    private boolean tracking;
    private boolean dragging;
    private boolean wasExpanded;
    private boolean scrollPending;
    private float downX;
    private float downY;
    private int dragHeight;

    private final OnLayoutChangeListener resizeWithWindow=(view,left,top,right,bottom,oldLeft,oldTop,oldRight,oldBottom)->{
        if(expanded&&!dragging&&animation==null&&bottom-top!=oldBottom-oldTop)
            post(()->{if(expanded&&!dragging&&animation==null)setBodyHeight(expandedHeight());});
    };
    private final ViewTreeObserver.OnPreDrawListener scrollAtDraw=()->{
        cancelScroll();
        if(log.getVisibility()==VISIBLE)log.scrollTo(0,log.getChildAt(0).getHeight());
        return true;
    };

    public JournalPanel(Context context,AttributeSet attrs) {
        super(context,attrs);
        touchSlop=ViewConfiguration.get(context).getScaledTouchSlop();
        swipeDistance=Math.max(2*touchSlop,dp(24));
    }

    @Override protected void onFinishInflate() {
        super.onFinishInflate();
        header=findViewById(id("journal_header"));
        body=findViewById(id("journal_body"));
        preview=findViewById(id("journal_preview"));
        log=findViewById(id("journal_scroll"));
        toggle=findViewById(id("journal"));
        toggle.setOnClickListener(view->setExpanded(!expanded));
        header.setOnClickListener(view->setExpanded(!expanded));
        preview.setOnClickListener(view->setExpanded(true));
    }

    private int id(String name) {
        return getResources().getIdentifier(name,"id",getContext().getPackageName());
    }

    private int dp(int value) {
        return Math.round(value*getResources().getDisplayMetrics().density);
    }

    @Override protected void onAttachedToWindow() {
        super.onAttachedToWindow();
        ((View)getParent()).addOnLayoutChangeListener(resizeWithWindow);
    }

    @Override protected void onDetachedFromWindow() {
        if(getParent() instanceof View)((View)getParent()).removeOnLayoutChangeListener(resizeWithWindow);
        cancelAnimation();
        cancelScroll();
        super.onDetachedFromWindow();
    }

    boolean isExpanded() {
        return expanded;
    }

    boolean followsLatest() {
        return expanded&&(scrollPending||!log.canScrollVertically(1));
    }

    void scrollToLatest() {
        if(scrollPending||!isAttachedToWindow())return;
        scrollPending=true;
        getViewTreeObserver().addOnPreDrawListener(scrollAtDraw);
        log.invalidate();
    }

    private void cancelScroll() {
        if(!scrollPending)return;
        getViewTreeObserver().removeOnPreDrawListener(scrollAtDraw);
        scrollPending=false;
    }

    void setExpanded(boolean open) {
        boolean opening=open&&!expanded;
        cancelAnimation();
        dragging=false;
        expanded=open;
        toggle.setText(open?"Свернуть":"Развернуть");
        toggle.setContentDescription(open?"Свернуть журнал":"Открыть весь журнал");
        int from=bodyHeight();
        int target=open?expandedHeight():preview.getMeasuredHeight();
        if(from==target){
            finishTransition();
            if(opening)scrollToLatest();
            return;
        }
        showLogAt(from);
        if(opening)scrollToLatest();
        animation=ValueAnimator.ofInt(from,target);
        animation.setDuration(210);
        animation.setInterpolator(new DecelerateInterpolator());
        animation.addUpdateListener(value->setBodyHeight((int)value.getAnimatedValue()));
        animation.addListener(new AnimatorListenerAdapter(){
            @Override public void onAnimationEnd(Animator finished) {
                if(animation==finished){
                    animation=null;
                    finishTransition();
                    if(opening)scrollToLatest();
                }
            }
        });
        animation.start();
    }

    private void finishTransition() {
        log.setVisibility(expanded?VISIBLE:GONE);
        preview.setVisibility(expanded?INVISIBLE:VISIBLE);
        setBodyHeight(expanded?expandedHeight():LayoutParams.WRAP_CONTENT);
    }

    private void showLogAt(int height) {
        setBodyHeight(height);
        preview.setVisibility(INVISIBLE);
        log.setVisibility(VISIBLE);
    }

    private int expandedHeight() {
        int windowHeight=((View)getParent()).getHeight();
        return Math.max(preview.getMeasuredHeight(),Math.round(windowHeight*0.58f));
    }

    private int bodyHeight() {
        int height=body.getLayoutParams().height;
        return height>=0?height:body.getHeight();
    }

    private void setBodyHeight(int height) {
        LayoutParams params=(LayoutParams)body.getLayoutParams();
        if(params.height==height)return;
        params.height=height;
        body.setLayoutParams(params);
    }

    private void cancelAnimation() {
        ValueAnimator running=animation;
        animation=null;
        if(running!=null)running.cancel();
    }

    @Override public boolean onInterceptTouchEvent(MotionEvent event) {
        switch(event.getActionMasked()){
            case MotionEvent.ACTION_DOWN:
                tracking=!expanded||event.getY()<=header.getBottom();
                downX=event.getRawX();
                downY=event.getRawY();
                break;
            case MotionEvent.ACTION_MOVE:
                if(!tracking)break;
                // The header moves as the sheet is dragged; compare screen coordinates.
                float delta=downY-event.getRawY();
                if(Math.abs(delta)>touchSlop&&Math.abs(delta)>Math.abs(event.getRawX()-downX)
                    &&(expanded?delta<0:delta>0)){
                    cancelAnimation();
                    wasExpanded=expanded;
                    dragHeight=bodyHeight();
                    dragging=true;
                    showLogAt(dragHeight);
                    if(!wasExpanded)scrollToLatest();
                    dragTo(delta);
                    return true;
                }
                break;
            case MotionEvent.ACTION_POINTER_DOWN:
            case MotionEvent.ACTION_CANCEL:
            case MotionEvent.ACTION_UP:
                tracking=false;
                break;
        }
        return super.onInterceptTouchEvent(event);
    }

    @Override public boolean onTouchEvent(MotionEvent event) {
        if(!dragging)return super.onTouchEvent(event);
        float delta=downY-event.getRawY();
        switch(event.getActionMasked()){
            case MotionEvent.ACTION_MOVE:
                dragTo(delta);
                break;
            case MotionEvent.ACTION_UP:
                setExpanded(wasExpanded?delta>-swipeDistance:delta>=swipeDistance);
                break;
            case MotionEvent.ACTION_POINTER_DOWN:
            case MotionEvent.ACTION_CANCEL:
                setExpanded(wasExpanded);
                break;
        }
        return true;
    }

    private void dragTo(float delta) {
        setBodyHeight(Math.max(preview.getMeasuredHeight(),Math.min(expandedHeight(),dragHeight+Math.round(delta))));
    }
}
